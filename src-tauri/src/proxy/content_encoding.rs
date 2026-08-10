//! HTTP content-encoding 工具。
//!
//! reqwest 的自动解压已禁用（为了透传 accept-encoding），需要手动解压。
//! 请求侧（如 Codex Desktop 在登录态发压缩请求体）与响应侧（上游压缩响应体）
//! 共用同一套解压逻辑。

use axum::http::header::HeaderMap;
use std::io::Read;

const LIMITED_DECODER_WINDOW_LOG_MAX: u32 = 23;

/// 把 content-encoding 值拆成有序 coding 列表（去掉 identity 与空值）。
///
/// HTTP 允许堆叠编码（如 `gzip, zstd`），各 coding 以逗号分隔；亦允许重复
/// content-encoding 头，语义等同逗号拼接（见 [`get_content_encoding`]）。
fn split_codings(content_encoding: &str) -> Vec<&str> {
    content_encoding
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "identity")
        .collect()
}

/// 单个 coding 是否可被解压。
fn is_single_supported(coding: &str) -> bool {
    matches!(
        coding,
        "gzip" | "x-gzip" | "deflate" | "br" | "zstd" | "zst"
    )
}

/// 解压失败原因。把「输出超预算」与「数据损坏」区分开：前者是安全拒绝信号，
/// 响应侧调用方应据此拒绝响应（502），而不是当成普通解压失败静默回退。
#[derive(Debug)]
pub(crate) enum DecompressError {
    /// 底层解码失败（数据损坏 / 格式不符）。
    Io(std::io::Error),
    /// 解压输出超过 `limit` 字节即中止；此时真实输出大小未知，只会大于 limit。
    TooLarge { limit: usize },
}

impl std::fmt::Display for DecompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::TooLarge { limit } => write!(f, "解压输出超过上限 {limit} 字节"),
        }
    }
}

impl std::error::Error for DecompressError {}

impl From<std::io::Error> for DecompressError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<DecompressError> for std::io::Error {
    fn from(e: DecompressError) -> Self {
        match e {
            DecompressError::Io(e) => e,
            DecompressError::TooLarge { limit } => {
                std::io::Error::other(format!("decompressed body exceeds {limit} bytes"))
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LimitedDecompressedBody {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

fn brotli_window_log(body: &[u8]) -> Result<u32, std::io::Error> {
    let first = *body.first().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "brotli header is incomplete",
        )
    })?;

    if first & 1 == 0 {
        return Ok(16);
    }
    if let Some(window_log) = match first & 0x0f {
        0x03 => Some(18),
        0x05 => Some(19),
        0x07 => Some(20),
        0x09 => Some(21),
        0x0b => Some(22),
        0x0d => Some(23),
        0x0f => Some(24),
        _ => None,
    } {
        return Ok(window_log);
    }
    if let Some(window_log) = match first & 0x7f {
        0x71 => Some(15),
        0x61 => Some(14),
        0x51 => Some(13),
        0x41 => Some(12),
        0x31 => Some(11),
        0x21 => Some(10),
        0x01 => Some(17),
        _ => None,
    } {
        return Ok(window_log);
    }
    if first & 0x80 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid brotli window bits",
        ));
    }

    let second = *body.get(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "brotli large-window header is incomplete",
        )
    })?;
    let window_log = u32::from(second & 0x3f);
    if !(10..=30).contains(&window_log) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid brotli large-window bits",
        ));
    }
    Ok(window_log)
}

fn validate_brotli_window(body: &[u8]) -> Result<(), std::io::Error> {
    let window_log = brotli_window_log(body)?;
    if window_log > LIMITED_DECODER_WINDOW_LOG_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "brotli window log {window_log} exceeds limit {LIMITED_DECODER_WINDOW_LOG_MAX}"
            ),
        ));
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn has_zlib_framing(body: &[u8]) -> Result<bool, std::io::Error> {
    let Some((&cmf, rest)) = body.split_first() else {
        return Err(invalid_data("deflate body is empty"));
    };
    let Some(&flg) = rest.first() else {
        return Err(invalid_data("deflate header is incomplete"));
    };
    let header = (u16::from(cmf) << 8) | u16::from(flg);
    Ok(cmf & 0x0f == 8 && cmf >> 4 <= 7 && header % 31 == 0)
}

fn decompress_deflate(
    body: &[u8],
    output_limit: Option<usize>,
    input_truncated: bool,
) -> Result<LimitedDecompressedBody, std::io::Error> {
    // RFC 9110 的 deflate 使用 zlib framing。只有 CMF/FLG 明确不构成
    // RFC 1950 header 时才兼容 raw deflate；合法 zlib header 后的损坏不得回退。
    let zlib_header = has_zlib_framing(body)?;
    let format = if zlib_header { "zlib" } else { "raw deflate" };
    let mut decoder = flate2::Decompress::new(zlib_header);
    let sentinel_limit = output_limit.map(|limit| limit.saturating_add(1));
    let mut output = Vec::with_capacity(output_limit.unwrap_or(8 * 1024).min(8 * 1024));
    let mut buffer = [0u8; 8 * 1024];

    loop {
        let output_capacity = sentinel_limit
            .map(|sentinel| sentinel.saturating_sub(output.len()))
            .unwrap_or(buffer.len())
            .min(buffer.len());
        if output_capacity == 0 {
            return Ok(LimitedDecompressedBody {
                bytes: output,
                truncated: true,
            });
        }

        let input_offset = decoder.total_in() as usize;
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let flush = if input_offset == body.len() {
            flate2::FlushDecompress::Finish
        } else {
            flate2::FlushDecompress::None
        };
        let result =
            decoder.decompress(&body[input_offset..], &mut buffer[..output_capacity], flush);
        let consumed = (decoder.total_in() - before_in) as usize;
        let produced = (decoder.total_out() - before_out) as usize;
        output.extend_from_slice(&buffer[..produced]);

        if let Some(limit) = output_limit {
            if output.len() > limit {
                output.truncate(limit);
                return Ok(LimitedDecompressedBody {
                    bytes: output,
                    truncated: true,
                });
            }
        }

        match result {
            Ok(flate2::Status::StreamEnd) => {
                if decoder.total_in() != body.len() as u64 {
                    return Err(invalid_data(format!("{format} stream has trailing bytes")));
                }
                if input_truncated && output.is_empty() {
                    return Err(invalid_data(format!(
                        "{format} truncated input produced no output"
                    )));
                }
                return Ok(LimitedDecompressedBody {
                    bytes: output,
                    truncated: input_truncated,
                });
            }
            Ok(flate2::Status::Ok | flate2::Status::BufError) => {}
            Err(error) => {
                if input_truncated && !output.is_empty() {
                    return Ok(LimitedDecompressedBody {
                        bytes: output,
                        truncated: true,
                    });
                }
                return Err(invalid_data(format!("invalid {format} stream: {error}")));
            }
        }

        if consumed == 0 && produced == 0 {
            if input_truncated && !output.is_empty() {
                return Ok(LimitedDecompressedBody {
                    bytes: output,
                    truncated: true,
                });
            }
            return Err(invalid_data(format!("incomplete {format} stream")));
        }
    }
}

fn read_prefix<R: Read>(
    mut reader: R,
    limit: usize,
    input_truncated: bool,
) -> Result<LimitedDecompressedBody, std::io::Error> {
    let sentinel_limit = limit.saturating_add(1);
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0u8; 8 * 1024];
    let mut saw_eof = false;

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(_) if input_truncated && !output.is_empty() => {
                return Ok(LimitedDecompressedBody {
                    bytes: output,
                    truncated: true,
                });
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            if saw_eof {
                return Ok(LimitedDecompressedBody {
                    bytes: output,
                    truncated: input_truncated,
                });
            }
            saw_eof = true;
            continue;
        }
        saw_eof = false;
        let remaining = sentinel_limit.saturating_sub(output.len());
        let take = read.min(remaining);
        output.extend_from_slice(&buffer[..take]);
        if output.len() > limit {
            output.truncate(limit);
            return Ok(LimitedDecompressedBody {
                bytes: output,
                truncated: true,
            });
        }
    }
}

fn decompress_brotli(
    body: &[u8],
    limit: usize,
    input_truncated: bool,
    enforce_window_limit: bool,
) -> Result<LimitedDecompressedBody, std::io::Error> {
    if enforce_window_limit {
        validate_brotli_window(body)?;
    }

    let cursor = std::io::Cursor::new(body);
    let mut decoder = brotli::Decompressor::new(cursor, 8 * 1024);
    let decoded = read_prefix(&mut decoder, limit, input_truncated)?;
    if !decoded.truncated && decoder.get_ref().position() != body.len() as u64 {
        return Err(invalid_data("brotli stream has trailing bytes"));
    }
    Ok(decoded)
}

fn decompress_single_limited(
    coding: &str,
    body: &[u8],
    limit: usize,
    input_truncated: bool,
) -> Result<Option<LimitedDecompressedBody>, std::io::Error> {
    match coding {
        "gzip" | "x-gzip" => read_prefix(
            flate2::read::MultiGzDecoder::new(body),
            limit,
            input_truncated,
        )
        .map(Some),
        "deflate" => decompress_deflate(body, Some(limit), input_truncated).map(Some),
        "br" => decompress_brotli(body, limit, input_truncated, true).map(Some),
        "zstd" | "zst" => {
            let mut decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(body))?;
            decoder.window_log_max(LIMITED_DECODER_WINDOW_LOG_MAX)?;
            read_prefix(decoder, limit, input_truncated).map(Some)
        }
        _ => Ok(None),
    }
}

/// 解压单个 content-coding，输出上限 `max_output_bytes`。未知编码返回 `Ok(None)`。
fn decompress_single(
    coding: &str,
    body: &[u8],
    max_output_bytes: usize,
) -> Result<Option<Vec<u8>>, DecompressError> {
    if max_output_bytes != usize::MAX {
        return decompress_single_limited(coding, body, max_output_bytes, false)
            .map_err(DecompressError::Io)?
            .map(|decoded| {
                if decoded.truncated {
                    Err(DecompressError::TooLarge {
                        limit: max_output_bytes,
                    })
                } else {
                    Ok(decoded.bytes)
                }
            })
            .transpose();
    }

    match coding {
        "gzip" | "x-gzip" => {
            let mut decoder = flate2::read::MultiGzDecoder::new(body);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;
            Ok(Some(decompressed))
        }
        "deflate" => decompress_deflate(body, None, false)
            .map(|decoded| Some(decoded.bytes))
            .map_err(DecompressError::Io),
        "br" => decompress_brotli(body, usize::MAX, false, false)
            .map(|decoded| Some(decoded.bytes))
            .map_err(DecompressError::Io),
        "zstd" | "zst" => {
            let decompressed = zstd::stream::decode_all(std::io::Cursor::new(body))?;
            Ok(Some(decompressed))
        }
        _ => Ok(None),
    }
}

/// 根据 content-encoding 解压 body 字节，支持堆叠编码（如 `gzip, zstd`），
/// 且每个 coding 的解压输出（含堆叠编码的中间产物）都受 `max_output_bytes`
/// 限制，超限即中止并返回 [`DecompressError::TooLarge`]，用于防御响应侧压缩炸弹。
///
/// RFC 9110 §8.4：codings 按**应用顺序**列出，故解压须**反向**（最后应用的先解）。
/// 返回 `Ok(None)` 表示存在不受支持的编码、原样透传——此时调用方必须保留
/// content-encoding 头，否则下游（诊断 / 客户端）会把压缩字节误当明文。
pub(crate) fn decompress_body_with_limit(
    content_encoding: &str,
    body: &[u8],
    max_output_bytes: usize,
) -> Result<Option<Vec<u8>>, DecompressError> {
    let codings = split_codings(content_encoding);
    if codings.is_empty() {
        return Ok(None);
    }
    // 任一 coding 不支持就整体放弃解压、保头透传，避免半解码的脏数据。
    if !codings.iter().all(|c| is_single_supported(c)) {
        log::warn!("不支持的 content-encoding: {content_encoding}，跳过解压");
        return Ok(None);
    }

    // 反向解码：列表末尾是最后应用的编码，须最先解。
    let mut data: Option<Vec<u8>> = None;
    for coding in codings.iter().rev() {
        let input = data.as_deref().unwrap_or(body);
        match decompress_single(coding, input, max_output_bytes)? {
            Some(decompressed) => data = Some(decompressed),
            // 上面 is_single_supported 已校验，理论不会发生；防御性兜底。
            None => return Ok(None),
        }
    }
    Ok(data)
}

/// 无输出上限的 [`decompress_body_with_limit`] 版本，供请求侧等已有自身
/// 体积约束的调用方使用。
pub(crate) fn decompress_body(
    content_encoding: &str,
    body: &[u8],
) -> Result<Option<Vec<u8>>, std::io::Error> {
    decompress_body_with_limit(content_encoding, body, usize::MAX).map_err(Into::into)
}

/// 有界解压错误正文。每一层解码输入与输出最多保留 `limit` 字节，
/// `initial_input_truncated` 表示调用方传入的已经是 wire body 前缀。
pub(crate) fn decompress_body_limited(
    content_encoding: &str,
    body: &[u8],
    limit: usize,
    initial_input_truncated: bool,
) -> Result<Option<LimitedDecompressedBody>, std::io::Error> {
    let codings = split_codings(content_encoding);
    if codings.is_empty() {
        return Ok(None);
    }
    if !codings.iter().all(|coding| is_single_supported(coding)) {
        log::warn!("不支持的 content-encoding: {content_encoding}，跳过有界解压");
        return Ok(None);
    }

    let initial_input = &body[..body.len().min(limit)];
    let mut data: Option<Vec<u8>> = None;
    let mut input_truncated = initial_input_truncated || body.len() > limit;
    for coding in codings.iter().rev() {
        let input = data.as_deref().unwrap_or(initial_input);
        let Some(decoded) = decompress_single_limited(coding, input, limit, input_truncated)?
        else {
            return Ok(None);
        };
        input_truncated = decoded.truncated;
        data = Some(decoded.bytes);
    }

    Ok(data.map(|bytes| LimitedDecompressedBody {
        bytes,
        truncated: input_truncated,
    }))
}

/// 该 content-encoding（含堆叠，如 `gzip, zstd`）是否全部可被解压。
///
/// 请求侧用它做闸门：无法解压的压缩体不能透传给 JSON 解析，需直接拒绝。
pub(crate) fn is_supported_content_encoding(content_encoding: &str) -> bool {
    let codings = split_codings(content_encoding);
    !codings.is_empty() && codings.iter().all(|c| is_single_supported(c))
}

/// 从 header 提取 content-encoding（合并重复头，忽略 identity 与空值）。
///
/// HTTP 允许重复 content-encoding 头，语义等同逗号拼接，故用 `get_all` 合并；
/// 返回值可能含多个逗号分隔的 coding，交由 [`decompress_body`] 反向解码。
pub(crate) fn get_content_encoding(headers: &HeaderMap) -> Option<String> {
    let combined = headers
        .get_all("content-encoding")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
        .to_lowercase();
    if split_codings(&combined).is_empty() {
        return None;
    }
    Some(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn gzip_member(payload: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli_stream(payload: &[u8]) -> Vec<u8> {
        let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.into_inner()
    }

    fn brotli_stream_with_window(payload: &[u8], window_log: u32) -> Vec<u8> {
        let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, window_log);
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.into_inner()
    }

    fn zlib_stream(payload: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.finish().unwrap()
    }

    fn raw_deflate_stream(payload: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.finish().unwrap()
    }

    fn deterministic_payload(len: usize) -> Vec<u8> {
        let mut state = 0x7a5b_31d2_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    }

    fn brotli_stream_with_exact_len(target_len: usize) -> (Vec<u8>, Vec<u8>) {
        let source = deterministic_payload(target_len + 256);
        for payload_len in target_len.saturating_sub(128)..=target_len + 128 {
            let payload = source[..payload_len].to_vec();
            let compressed = brotli_stream(&payload);
            if compressed.len() == target_len {
                return (payload, compressed);
            }
        }
        panic!("failed to construct deterministic {target_len}-byte brotli stream");
    }

    #[test]
    fn decompress_body_gzip_reads_all_members() {
        let mut compressed = gzip_member(b"first-");
        compressed.extend_from_slice(&gzip_member(b"second"));

        for coding in ["gzip", "x-gzip"] {
            let decompressed = decompress_body(coding, &compressed).unwrap().unwrap();
            assert_eq!(decompressed, b"first-second", "{coding}");
        }
    }

    #[test]
    fn decompress_body_limited_gzip_reads_all_members() {
        let mut compressed = gzip_member(b"first-");
        compressed.extend_from_slice(&gzip_member(b"second"));

        for coding in ["gzip", "x-gzip"] {
            let decompressed = decompress_body_limited(coding, &compressed, 1024, false)
                .unwrap()
                .unwrap();
            assert_eq!(decompressed.bytes, b"first-second", "{coding}");
            assert!(!decompressed.truncated, "{coding}");
        }
    }

    #[test]
    fn decompress_body_deflate_handles_zlib_wrapped_per_rfc9110() {
        // RFC 9110 规范的 deflate = zlib 包裹格式（合规来源发的就是这个）
        let payload = br#"{"ok":true}"#;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let decompressed = decompress_body("deflate", &compressed).unwrap().unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn decompress_body_deflate_falls_back_to_raw_stream() {
        // 部分来源违规发 raw deflate 流，保持兼容
        let payload = br#"{"ok":true}"#;
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let decompressed = decompress_body("deflate", &compressed).unwrap().unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn decompress_body_limited_deflate_distinguishes_exact_limit_from_expansion() {
        const LIMIT: usize = 8 * 1024;
        let encoders: [(&str, fn(&[u8]) -> Vec<u8>); 2] =
            [("zlib", zlib_stream), ("raw", raw_deflate_stream)];

        for (format, encode) in encoders {
            for payload_len in [LIMIT, LIMIT + 1] {
                let payload = vec![b'x'; payload_len];
                let compressed = encode(&payload);
                assert!(
                    compressed.len() < LIMIT,
                    "{format} fixture must exercise the decoded-output cap"
                );

                let decoded = decompress_body_limited("deflate", &compressed, LIMIT, false)
                    .unwrap()
                    .unwrap();

                assert_eq!(decoded.bytes, payload[..LIMIT], "{format} {payload_len}");
                assert_eq!(
                    decoded.truncated,
                    payload_len > LIMIT,
                    "{format} {payload_len}"
                );
            }
        }
    }

    #[test]
    fn decompress_body_deflate_rejects_zlib_trailing_garbage() {
        let compressed = [zlib_stream(b"hello").as_slice(), b"trailing"].concat();

        let error = decompress_body("deflate", &compressed).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_deflate_rejects_raw_trailing_garbage() {
        let compressed = [raw_deflate_stream(b"hello").as_slice(), b"trailing"].concat();

        let error = decompress_body("deflate", &compressed).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_deflate_rejects_empty_body() {
        let error = decompress_body("deflate", b"").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_deflate_rejects_zlib_trailing_garbage() {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, b"hello").unwrap();
        let compressed = [encoder.finish().unwrap().as_slice(), b"trailing"].concat();

        let error = decompress_body_limited("deflate", &compressed, 1024, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_deflate_rejects_raw_trailing_garbage() {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, b"hello").unwrap();
        let compressed = [encoder.finish().unwrap().as_slice(), b"trailing"].concat();

        let error = decompress_body_limited("deflate", &compressed, 1024, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_deflate_rejects_truncated_zlib_adler() {
        let mut compressed = zlib_stream(&deterministic_payload(4096));
        compressed.pop();

        let error = decompress_body_limited("deflate", &compressed, 8192, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_deflate_rejects_truncated_zlib_payload() {
        let mut compressed = zlib_stream(&deterministic_payload(4096));
        compressed.truncate(compressed.len() / 2);

        let error = decompress_body_limited("deflate", &compressed, 8192, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_deflate_rejects_truncated_raw_stream() {
        let mut compressed = raw_deflate_stream(&deterministic_payload(4096));
        compressed.pop();

        let error = decompress_body_limited("deflate", &compressed, 8192, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_deflate_allows_truncated_input_after_output() {
        let payload = deterministic_payload(4096);
        let mut compressed = zlib_stream(&payload);
        compressed.truncate(compressed.len() - 4);

        let decoded = decompress_body_limited("deflate", &compressed, 8192, true)
            .unwrap()
            .unwrap();

        assert!(decoded.truncated);
        assert!(!decoded.bytes.is_empty());
        assert!(payload.starts_with(&decoded.bytes));
    }

    #[test]
    fn decompress_body_limited_deflate_rejects_truncated_input_without_output() {
        let compressed = zlib_stream(b"hello");

        let error = decompress_body_limited("deflate", &compressed[..2], 1024, true).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_rejects_truncated_inner_raw_deflate() {
        let mut inner = raw_deflate_stream(&deterministic_payload(4096));
        inner.pop();
        let stacked = zstd::stream::encode_all(std::io::Cursor::new(inner), 0).unwrap();

        let error = decompress_body_limited("deflate, zstd", &stacked, 8192, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_zstd_roundtrip() {
        // Codex 登录态发的就是 zstd 压缩请求体
        let payload = br#"{"hello":"world","n":42}"#;
        let compressed = zstd::stream::encode_all(std::io::Cursor::new(&payload[..]), 0).unwrap();
        let decompressed = decompress_body("zstd", &compressed).unwrap().unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn brotli_window_preflight_rejects_over_budget_header() {
        validate_brotli_window(&[0x0b]).unwrap();

        let error = validate_brotli_window(&[0x11, 30]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_limited_brotli_rejects_valid_window_24_stream() {
        let payload = b"valid brotli window 24 stream";
        let compressed = brotli_stream_with_window(payload, 24);
        assert_eq!(brotli_window_log(&compressed).unwrap(), 24);

        let unrestricted = decompress_body("br", &compressed).unwrap().unwrap();
        assert_eq!(unrestricted, payload);

        let error = decompress_body_limited("br", &compressed, 1024, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("window log 24 exceeds limit 23"),
            "{error}"
        );
    }

    #[test]
    fn decompress_body_limited_brotli_rejects_trailing_garbage() {
        let compressed = brotli_stream(b"hello");
        let decoded = decompress_body_limited("br", &compressed, 1024, false)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.bytes, b"hello");
        assert!(!decoded.truncated);

        let with_trailing = [compressed.as_slice(), b"trailing garbage"].concat();
        let error = decompress_body_limited("br", &with_trailing, 1024, false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_brotli_rejects_trailing_garbage() {
        let compressed = [brotli_stream(b"hello").as_slice(), b"trailing garbage"].concat();

        let error = decompress_body("br", &compressed).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decompress_body_brotli_rejects_trailing_after_exact_input_buffers() {
        for compressed_len in [8 * 1024, 16 * 1024] {
            let (payload, compressed) = brotli_stream_with_exact_len(compressed_len);
            let limit = compressed_len * 2;

            let unrestricted = decompress_body("br", &compressed).unwrap().unwrap();
            assert_eq!(unrestricted, payload, "{compressed_len}");
            let limited = decompress_body_limited("br", &compressed, limit, false)
                .unwrap()
                .unwrap();
            assert_eq!(limited.bytes, payload, "{compressed_len}");
            assert!(!limited.truncated, "{compressed_len}");

            let with_trailing = [compressed.as_slice(), b"trailing"].concat();
            let unrestricted_error = decompress_body("br", &with_trailing).unwrap_err();
            assert_eq!(
                unrestricted_error.kind(),
                std::io::ErrorKind::InvalidData,
                "{compressed_len}"
            );
            let limited_error =
                decompress_body_limited("br", &with_trailing, limit, false).unwrap_err();
            assert_eq!(
                limited_error.kind(),
                std::io::ErrorKind::InvalidData,
                "{compressed_len}"
            );
        }
    }

    #[test]
    fn decompress_body_limited_zstd_rejects_over_budget_window() {
        let over_budget_empty_frame = [
            0x28, 0xb5, 0x2f, 0xfd, // magic
            0x00, // frame header descriptor: non-single-segment, no content size
            0x70, // window descriptor: 1 << 24
            0x01, 0x00, 0x00, // empty last raw block
        ];

        let unrestricted = decompress_body("zstd", &over_budget_empty_frame)
            .unwrap()
            .unwrap();
        assert!(unrestricted.is_empty());

        let error =
            decompress_body_limited("zstd", &over_budget_empty_frame, 1024, false).unwrap_err();
        let message = error.to_string().to_ascii_lowercase();
        assert!(
            message.contains("window") || message.contains("memory"),
            "{error}"
        );
    }

    #[test]
    fn decompress_body_limited_zstd_rejects_trailing_garbage() {
        let compressed = zstd::stream::encode_all(std::io::Cursor::new(b"hello"), 0).unwrap();
        let with_trailing = [compressed.as_slice(), b"trailing"].concat();

        decompress_body_limited("zstd", &with_trailing, 1024, false).unwrap_err();
    }

    #[test]
    fn decompress_body_stacked_gzip_then_zstd_decodes_in_reverse() {
        // Content-Encoding: gzip, zstd 表示先 gzip 后 zstd，解压须反向（先 zstd 后 gzip）
        let payload = br#"{"stacked":true}"#;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, payload).unwrap();
        let gzipped = gz.finish().unwrap();
        let stacked = zstd::stream::encode_all(std::io::Cursor::new(&gzipped[..]), 0).unwrap();

        let decompressed = decompress_body("gzip, zstd", &stacked).unwrap().unwrap();
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn decompress_body_stacked_with_unsupported_returns_none() {
        // 堆叠里只要有一个不支持，就整体保头透传
        let result = decompress_body("snappy, zstd", b"\x00\x01\x02\x03").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn decompress_body_limited_caps_gzip_expansion() {
        let payload = vec![b'x'; 64 * 1024];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let decompressed = decompress_body_limited("gzip", &compressed, 1024, false)
            .unwrap()
            .unwrap();
        assert_eq!(decompressed.bytes.len(), 1024);
        assert!(decompressed.truncated);
    }

    #[test]
    fn decompress_body_limited_caps_brotli_expansion() {
        let payload = vec![b'x'; 64 * 1024];
        let compressed = brotli_stream(&payload);

        let decompressed = decompress_body_limited("br", &compressed, 1024, false)
            .unwrap()
            .unwrap();

        assert_eq!(decompressed.bytes.len(), 1024);
        assert!(decompressed.truncated);
        assert!(payload.starts_with(&decompressed.bytes));
    }

    #[test]
    fn decompress_body_limited_caps_zstd_expansion() {
        let payload = vec![b'x'; 64 * 1024];
        let compressed = zstd::stream::encode_all(std::io::Cursor::new(&payload), 0).unwrap();

        let decompressed = decompress_body_limited("zstd", &compressed, 1024, false)
            .unwrap()
            .unwrap();

        assert_eq!(decompressed.bytes.len(), 1024);
        assert!(decompressed.truncated);
        assert!(payload.starts_with(&decompressed.bytes));
    }

    #[test]
    fn decompress_body_limited_preserves_small_payload() {
        let payload = br#"{"ok":true}"#;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let decompressed = decompress_body_limited("gzip", &compressed, 1024, false)
            .unwrap()
            .unwrap();
        assert_eq!(decompressed.bytes, payload);
        assert!(!decompressed.truncated);
    }

    #[test]
    fn decompress_body_limited_stacked_small_payload_roundtrip() {
        let payload = br#"{"stacked":true}"#;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gzip, payload).unwrap();
        let gzipped = gzip.finish().unwrap();
        let stacked = zstd::stream::encode_all(std::io::Cursor::new(gzipped), 0).unwrap();

        let decompressed = decompress_body_limited("gzip, zstd", &stacked, 1024, false)
            .unwrap()
            .unwrap();

        assert_eq!(decompressed.bytes, payload);
        assert!(!decompressed.truncated);
    }

    #[test]
    fn decompress_body_limited_stacked_truncation_preserves_decoded_prefix() {
        let payload = vec![b'x'; 4096];
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
        std::io::Write::write_all(&mut gzip, &payload).unwrap();
        let gzipped = gzip.finish().unwrap();
        let stacked = zstd::stream::encode_all(std::io::Cursor::new(gzipped), 0).unwrap();
        let limit = 128;
        assert!(stacked.len() <= limit);

        let decompressed = decompress_body_limited("gzip, zstd", &stacked, limit, false)
            .unwrap()
            .unwrap();

        assert!(decompressed.truncated);
        assert!(!decompressed.bytes.is_empty());
        assert!(decompressed.bytes.len() <= limit);
        assert!(payload.starts_with(&decompressed.bytes));
    }

    #[test]
    fn decompress_body_limited_truncated_wire_preserves_decoded_prefix() {
        let payload = vec![b'x'; 4096];
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
        std::io::Write::write_all(&mut gzip, &payload).unwrap();
        let compressed = gzip.finish().unwrap();
        let limit = 128;
        assert!(compressed.len() > limit);

        let decompressed = decompress_body_limited("gzip", &compressed[..limit], limit, true)
            .unwrap()
            .unwrap();

        assert!(decompressed.truncated);
        assert!(!decompressed.bytes.is_empty());
        assert!(decompressed.bytes.len() <= limit);
        assert!(payload.starts_with(&decompressed.bytes));
    }

    #[test]
    fn decompress_body_unknown_encoding_returns_none_to_keep_headers() {
        // 未知编码必须返回 None（而非伪装成"已解码"），否则 content-encoding
        // 头被剥掉，下游诊断会把压缩字节误报成明文
        let result = decompress_body("snappy", b"\x00\x01\x02\x03").unwrap();
        assert!(result.is_none());
    }

    /// 生成确定性伪随机字节（LCG），避免测试引入 rand 依赖。
    fn pseudo_random_bytes(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x243F_6A88_85A3_08D3;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    fn gzip_compress(payload: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn decompress_body_with_limit_passes_payload_under_limit() {
        let payload = br#"{"ok":true}"#;
        let compressed = gzip_compress(payload);

        let out = decompress_body_with_limit("gzip", &compressed, 1024)
            .unwrap()
            .unwrap();
        assert_eq!(out, payload);
    }

    #[test]
    fn decompress_body_with_limit_allows_exactly_limit_bytes() {
        let payload = vec![7u8; 64 * 1024];
        let compressed = gzip_compress(&payload);

        let out = decompress_body_with_limit("gzip", &compressed, 64 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(out.len(), 64 * 1024);
        assert_eq!(out, payload);
    }

    #[test]
    fn decompress_body_with_limit_aborts_gzip_bomb_mid_stream() {
        // 4 MiB 伪随机数据（压缩率约 1:1）gzip 后截断到 2 MiB：流在产出约 2 MiB
        // 解压数据后 abrupt 结束。有界读取应在 1 MiB 预算耗尽处报 TooLarge；
        // 无界读取会一路读到残缺的流尾报 UnexpectedEof（Io）——两者可区分，
        // 因此该测试能识别"先完整展开再比较"的退化。
        let payload = pseudo_random_bytes(4 * 1024 * 1024);
        let compressed = gzip_compress(&payload);
        assert!(compressed.len() > 2 * 1024 * 1024);
        let truncated = &compressed[..2 * 1024 * 1024];

        let result = decompress_body_with_limit("gzip", truncated, 1024 * 1024);
        assert!(
            matches!(result, Err(DecompressError::TooLarge { .. })),
            "应在预算耗尽处截停（TooLarge），而不是读到流尾才报错: {:?}",
            result.as_ref().map(|o| o.as_ref().map(Vec::len))
        );
    }

    #[test]
    fn decompress_body_with_limit_rejects_zstd_bomb() {
        // 高压缩比 payload：8 MiB 全零 → zstd 压缩后仅数 KiB，完整展开必然超限
        let payload = vec![0u8; 8 * 1024 * 1024];
        let compressed = zstd::stream::encode_all(std::io::Cursor::new(&payload[..]), 0).unwrap();
        assert!(compressed.len() < 1024 * 1024);

        let result = decompress_body_with_limit("zstd", &compressed, 1024 * 1024);
        assert!(
            matches!(result, Err(DecompressError::TooLarge { .. })),
            "zstd 压缩炸弹应在预算耗尽处截停: {:?}",
            result.as_ref().map(|o| o.as_ref().map(Vec::len))
        );
    }

    #[test]
    fn decompress_body_with_limit_rejects_brotli_bomb() {
        let payload = vec![0u8; 8 * 1024 * 1024];
        let mut compressed = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            std::io::Write::write_all(&mut writer, &payload).unwrap();
        }
        assert!(compressed.len() < 1024 * 1024);

        let result = decompress_body_with_limit("br", &compressed, 1024 * 1024);
        assert!(
            matches!(result, Err(DecompressError::TooLarge { .. })),
            "brotli 压缩炸弹应在预算耗尽处截停: {:?}",
            result.as_ref().map(|o| o.as_ref().map(Vec::len))
        );
    }

    #[test]
    fn decompress_body_with_limit_bounds_intermediate_stage_of_stacked_encodings() {
        // 堆叠编码 gzip, zstd：zstd 先解出 gzip 流（小），gzip 再展开成 8 MiB。
        // 中间产物同样受预算约束，不能只在最后一级设防。
        let payload = vec![0u8; 8 * 1024 * 1024];
        let gzipped = gzip_compress(&payload);
        let stacked = zstd::stream::encode_all(std::io::Cursor::new(&gzipped[..]), 0).unwrap();

        let result = decompress_body_with_limit("gzip, zstd", &stacked, 1024 * 1024);
        assert!(
            matches!(result, Err(DecompressError::TooLarge { .. })),
            "堆叠编码的中间解压产物也应受预算约束: {:?}",
            result.as_ref().map(|o| o.as_ref().map(Vec::len))
        );
    }

    #[test]
    fn is_supported_content_encoding_matches_decompressable() {
        for enc in [
            "gzip",
            "x-gzip",
            "deflate",
            "br",
            "zstd",
            "zst",
            "gzip, zstd",
        ] {
            assert!(is_supported_content_encoding(enc), "{enc} 应受支持");
        }
        for enc in ["identity", "snappy", "compress", "", "gzip, snappy"] {
            assert!(!is_supported_content_encoding(enc), "{enc} 不应受支持");
        }
    }

    #[test]
    fn get_content_encoding_combines_repeated_headers() {
        // 重复的 content-encoding 头等同逗号拼接，须用 get_all 合并
        let mut headers = HeaderMap::new();
        headers.append("content-encoding", HeaderValue::from_static("gzip"));
        headers.append("content-encoding", HeaderValue::from_static("zstd"));
        assert_eq!(
            get_content_encoding(&headers).as_deref(),
            Some("gzip, zstd")
        );
    }

    #[test]
    fn get_content_encoding_ignores_identity_only() {
        let mut headers = HeaderMap::new();
        headers.append("content-encoding", HeaderValue::from_static("identity"));
        assert_eq!(get_content_encoding(&headers), None);
    }
}
