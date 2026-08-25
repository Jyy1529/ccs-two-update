#[inline]
pub(crate) fn strip_sse_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    line.strip_prefix(&format!("{field}: "))
        .or_else(|| line.strip_prefix(&format!("{field}:")))
}

#[derive(Debug, Default)]
pub(crate) struct SseBlockCursor {
    block_start: usize,
    search_offset: usize,
    #[cfg(test)]
    examined_bytes: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SseScanResult<'a> {
    Block(&'a str),
    NeedMoreData,
    BudgetExhausted,
}

impl SseBlockCursor {
    pub(crate) fn next_block_budgeted<'a>(
        &mut self,
        buffer: &'a str,
        remaining_budget: &mut usize,
    ) -> SseScanResult<'a> {
        if *remaining_budget == 0 {
            return SseScanResult::BudgetExhausted;
        }

        let bytes = buffer.as_bytes();
        let mut offset = self.search_offset.max(self.block_start).min(bytes.len());

        while offset < bytes.len() {
            if *remaining_budget == 0 {
                self.search_offset = offset;
                return SseScanResult::BudgetExhausted;
            }
            *remaining_budget -= 1;

            #[cfg(test)]
            {
                self.examined_bytes += 1;
            }

            let delimiter_len = if bytes[offset..].starts_with(b"\r\n\r\n") {
                4
            } else if bytes[offset..].starts_with(b"\n\n") {
                2
            } else {
                offset += 1;
                continue;
            };

            let block_start = self.block_start;
            self.block_start = offset + delimiter_len;
            self.search_offset = self.block_start;
            return SseScanResult::Block(&buffer[block_start..offset]);
        }

        self.search_offset = bytes.len().saturating_sub(3).max(self.block_start);
        SseScanResult::NeedMoreData
    }

    pub(crate) fn remaining<'a>(&self, buffer: &'a str) -> &'a str {
        &buffer[self.block_start.min(buffer.len())..]
    }

    #[cfg(test)]
    fn examined_bytes(&self) -> usize {
        self.examined_bytes
    }
}

#[inline]
pub(crate) fn take_sse_block(buffer: &mut String) -> Option<String> {
    let mut best: Option<(usize, usize)> = None;

    for (delimiter, len) in [("\r\n\r\n", 4usize), ("\n\n", 2usize)] {
        if let Some(pos) = buffer.find(delimiter) {
            if best.is_none_or(|(best_pos, _)| pos < best_pos) {
                best = Some((pos, len));
            }
        }
    }

    let (pos, len) = best?;
    let block = buffer[..pos].to_string();
    buffer.drain(..pos + len);
    Some(block)
}

/// Append raw bytes to a UTF-8 `String` buffer, correctly handling multi-byte
/// characters that are split across chunk boundaries.
///
/// `remainder` accumulates trailing bytes from the previous chunk that form an
/// incomplete UTF-8 sequence (at most 3 bytes under normal operation). On each
/// call the remainder is prepended to `new_bytes`, the longest valid UTF-8
/// prefix is appended to `buffer`, and any trailing incomplete bytes are saved
/// back into `remainder` for the next call.
///
/// A defensive guard discards `remainder` via lossy conversion if it ever
/// exceeds 3 bytes, which cannot happen with well-formed UTF-8 streams.
pub(crate) fn append_utf8_safe(buffer: &mut String, remainder: &mut Vec<u8>, new_bytes: &[u8]) {
    // Build the byte slice to decode: prepend any leftover bytes from previous chunk.
    let (owned, bytes): (Option<Vec<u8>>, &[u8]) = if remainder.is_empty() {
        (None, new_bytes)
    } else {
        // Defensive guard: remainder should never exceed 3 bytes (max incomplete
        // UTF-8 sequence is 3 bytes: a 4-byte char missing its last byte). If it
        // does, the stream is producing genuinely invalid bytes; flush them lossy
        // and start fresh.
        if remainder.len() > 3 {
            buffer.push_str(&String::from_utf8_lossy(remainder));
            remainder.clear();
            (None, new_bytes)
        } else {
            let mut combined = std::mem::take(remainder);
            combined.extend_from_slice(new_bytes);
            (Some(combined), &[])
        }
    };
    let input = owned.as_deref().unwrap_or(bytes);

    // Decode loop: consume all valid UTF-8 and any genuinely invalid bytes,
    // only leaving a trailing incomplete sequence in remainder.
    let mut pos = 0;
    loop {
        match std::str::from_utf8(&input[pos..]) {
            Ok(s) => {
                buffer.push_str(s);
                // Everything consumed – remainder stays empty.
                return;
            }
            Err(e) => {
                let valid_up_to = pos + e.valid_up_to();
                let valid_slice = &input[pos..valid_up_to];
                match std::str::from_utf8(valid_slice) {
                    Ok(valid) => buffer.push_str(valid),
                    Err(_) => buffer.push_str(&String::from_utf8_lossy(valid_slice)),
                }
                if let Some(invalid_len) = e.error_len() {
                    // Genuinely invalid byte(s) – emit U+FFFD and continue.
                    buffer.push('\u{FFFD}');
                    pos = valid_up_to + invalid_len;
                } else {
                    // Incomplete trailing sequence – stash for next chunk.
                    *remainder = input[valid_up_to..].to_vec();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{append_utf8_safe, strip_sse_field, SseBlockCursor, SseScanResult};

    #[test]
    fn strip_sse_field_accepts_optional_space() {
        assert_eq!(
            strip_sse_field("data: {\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("event: message_start", "event"),
            Some("message_start")
        );
        assert_eq!(
            strip_sse_field("event:message_start", "event"),
            Some("message_start")
        );
        assert_eq!(strip_sse_field("id:1", "data"), None);
    }

    #[test]
    fn sse_block_cursor_supports_lf_delimiters() {
        let buffer = "data: {\"ok\":true}\n\nrest";
        let mut cursor = SseBlockCursor::default();
        let mut budget = usize::MAX;

        assert_eq!(
            cursor.next_block_budgeted(buffer, &mut budget),
            SseScanResult::Block("data: {\"ok\":true}")
        );
        assert_eq!(cursor.remaining(buffer), "rest");
    }

    #[test]
    fn sse_block_cursor_supports_crlf_delimiters() {
        let buffer = "data: {\"ok\":true}\r\n\r\nrest";
        let mut cursor = SseBlockCursor::default();
        let mut budget = usize::MAX;

        assert_eq!(
            cursor.next_block_budgeted(buffer, &mut budget),
            SseScanResult::Block("data: {\"ok\":true}")
        );
        assert_eq!(cursor.remaining(buffer), "rest");
    }

    #[test]
    fn sse_block_cursor_finds_delimiter_split_across_appends() {
        let mut buffer = "data: {\"ok\":true}\r\n\r".to_string();
        let mut cursor = SseBlockCursor::default();
        let mut budget = usize::MAX;

        assert_eq!(
            cursor.next_block_budgeted(&buffer, &mut budget),
            SseScanResult::NeedMoreData
        );
        buffer.push_str("\nrest");
        assert_eq!(
            cursor.next_block_budgeted(&buffer, &mut budget),
            SseScanResult::Block("data: {\"ok\":true}")
        );
        assert_eq!(cursor.remaining(&buffer), "rest");
    }

    #[test]
    fn sse_block_cursor_scans_large_empty_event_batch_linearly_with_budget_exhaustion() {
        let buffer = "\n\n".repeat(128 * 1024);
        let mut cursor = SseBlockCursor::default();
        let mut blocks = 0usize;
        let mut budget_exhaustions = 0usize;
        let mut budget = 16 * 1024;

        loop {
            match cursor.next_block_budgeted(&buffer, &mut budget) {
                SseScanResult::Block(block) => {
                    assert!(block.is_empty());
                    blocks += 1;
                }
                SseScanResult::BudgetExhausted => {
                    budget_exhaustions += 1;
                    budget = 16 * 1024;
                }
                SseScanResult::NeedMoreData => break,
            }
        }

        assert_eq!(blocks, 128 * 1024);
        assert!(budget_exhaustions > 0);
        assert!(cursor.remaining(&buffer).is_empty());
        assert!(cursor.examined_bytes() <= buffer.len());
    }

    #[test]
    fn sse_block_cursor_stops_at_budget_without_a_delimiter() {
        let buffer = "x".repeat(32 * 1024);
        let mut cursor = SseBlockCursor::default();
        let mut budget = 16 * 1024;

        assert_eq!(
            cursor.next_block_budgeted(&buffer, &mut budget),
            SseScanResult::BudgetExhausted
        );
        assert_eq!(cursor.examined_bytes(), 16 * 1024);

        budget = 16 * 1024;
        assert_eq!(
            cursor.next_block_budgeted(&buffer, &mut budget),
            SseScanResult::NeedMoreData
        );
        assert_eq!(cursor.examined_bytes(), buffer.len());
    }

    // ------------------------------------------------------------------
    // append_utf8_safe tests
    // ------------------------------------------------------------------

    #[test]
    fn ascii_passthrough() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"hello world");
        assert_eq!(buf, "hello world");
        assert!(rem.is_empty());
    }

    #[test]
    fn complete_multibyte_in_single_chunk() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, "你好世界".as_bytes());
        assert_eq!(buf, "你好世界");
        assert!(rem.is_empty());
    }

    #[test]
    fn split_multibyte_across_two_chunks() {
        // "你" = E4 BD A0 (3 bytes)
        let bytes = "你".as_bytes();
        assert_eq!(bytes.len(), 3);

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Chunk 1: first 2 bytes (incomplete)
        append_utf8_safe(&mut buf, &mut rem, &bytes[..2]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 2);

        // Chunk 2: last byte completes the character
        append_utf8_safe(&mut buf, &mut rem, &bytes[2..]);
        assert_eq!(buf, "你");
        assert!(rem.is_empty());
    }

    #[test]
    fn split_four_byte_char_across_chunks() {
        // 😀 = F0 9F 98 80 (4 bytes)
        let bytes = "😀".as_bytes();
        assert_eq!(bytes.len(), 4);

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Send 1 byte at a time
        append_utf8_safe(&mut buf, &mut rem, &bytes[..1]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 1);

        append_utf8_safe(&mut buf, &mut rem, &bytes[1..2]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 2);

        append_utf8_safe(&mut buf, &mut rem, &bytes[2..3]);
        assert_eq!(buf, "");
        assert_eq!(rem.len(), 3);

        append_utf8_safe(&mut buf, &mut rem, &bytes[3..]);
        assert_eq!(buf, "😀");
        assert!(rem.is_empty());
    }

    #[test]
    fn mixed_ascii_and_split_multibyte() {
        // "hi你" = 68 69 E4 BD A0
        let all = "hi你".as_bytes();
        assert_eq!(all.len(), 5);

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Chunk 1: "hi" + first byte of "你"
        append_utf8_safe(&mut buf, &mut rem, &all[..3]);
        assert_eq!(buf, "hi");
        assert_eq!(rem.len(), 1);

        // Chunk 2: remaining 2 bytes of "你"
        append_utf8_safe(&mut buf, &mut rem, &all[3..]);
        assert_eq!(buf, "hi你");
        assert!(rem.is_empty());
    }

    #[test]
    fn multiple_split_characters_in_sequence() {
        let text = "你好";
        let bytes = text.as_bytes(); // E4 BD A0 E5 A5 BD

        let mut buf = String::new();
        let mut rem = Vec::new();

        // Split in the middle: first char complete + 1 byte of second
        append_utf8_safe(&mut buf, &mut rem, &bytes[..4]);
        assert_eq!(buf, "你");
        assert_eq!(rem.len(), 1);

        // Remaining 2 bytes complete second char
        append_utf8_safe(&mut buf, &mut rem, &bytes[4..]);
        assert_eq!(buf, "你好");
        assert!(rem.is_empty());
    }

    #[test]
    fn empty_chunks_are_harmless() {
        let mut buf = String::new();
        let mut rem = Vec::new();

        append_utf8_safe(&mut buf, &mut rem, b"");
        assert_eq!(buf, "");
        assert!(rem.is_empty());

        append_utf8_safe(&mut buf, &mut rem, b"ok");
        assert_eq!(buf, "ok");

        append_utf8_safe(&mut buf, &mut rem, b"");
        assert_eq!(buf, "ok");
    }

    #[test]
    fn sse_json_with_chinese_split_at_boundary() {
        // Simulates an SSE data line with Chinese content split across chunks
        let json_line = "data: {\"text\":\"你好\"}\n\n";
        let bytes = json_line.as_bytes();

        // Find where "你" starts in the byte stream and split there
        let ni_start = bytes.windows(3).position(|w| w == "你".as_bytes()).unwrap();
        let split_point = ni_start + 1; // split inside "你"

        let mut buf = String::new();
        let mut rem = Vec::new();

        append_utf8_safe(&mut buf, &mut rem, &bytes[..split_point]);
        append_utf8_safe(&mut buf, &mut rem, &bytes[split_point..]);

        assert_eq!(buf, json_line);
        assert!(rem.is_empty());

        // Verify the buffer can be parsed as SSE with valid JSON
        let data = strip_sse_field(buf.lines().next().unwrap(), "data").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["text"], "你好");
    }

    #[test]
    fn invalid_bytes_flushed_immediately_not_accumulated() {
        // 0xFF is never valid in UTF-8 – it should be replaced immediately,
        // not stashed in remainder.
        let mut buf = String::new();
        let mut rem = Vec::new();

        // "hi" + invalid byte + "ok"
        append_utf8_safe(&mut buf, &mut rem, b"hi\xFFok");
        assert!(
            rem.is_empty(),
            "remainder should be empty after invalid byte"
        );
        assert!(buf.contains("hi"), "valid prefix must be present");
        assert!(buf.contains("ok"), "valid suffix must be present");
        assert!(buf.contains('\u{FFFD}'), "invalid byte must produce U+FFFD");
    }

    #[test]
    fn invalid_byte_in_slow_path_flushed_immediately() {
        let mut buf = String::new();
        let mut rem = Vec::new();

        // Prime remainder with an incomplete sequence (first byte of "你")
        append_utf8_safe(&mut buf, &mut rem, &"你".as_bytes()[..1]);
        assert_eq!(rem.len(), 1);

        // Next chunk starts with an invalid byte – the stale remainder and the
        // invalid byte should both be flushed, not accumulated.
        append_utf8_safe(&mut buf, &mut rem, b"\xFFworld");
        assert!(rem.is_empty(), "remainder should be empty");
        assert!(
            buf.contains("world"),
            "valid data after invalid byte must appear"
        );
    }

    #[test]
    fn defensive_guard_flushes_oversized_remainder() {
        let mut buf = String::new();
        let mut rem = Vec::new();

        // Manually inject 4 invalid bytes into remainder to trigger the >3 guard.
        // This can't happen with well-formed UTF-8, but tests the safety net.
        rem.extend_from_slice(b"\x80\x80\x80\x80");
        assert_eq!(rem.len(), 4);

        append_utf8_safe(&mut buf, &mut rem, b"hello");
        // The 4 invalid bytes should have been flushed lossy, then "hello" decoded.
        assert!(rem.is_empty(), "remainder must be empty after guard flush");
        assert!(
            buf.contains("hello"),
            "valid data after guard flush must appear"
        );
        // The 4 invalid bytes each produce a U+FFFD
        let replacement_count = buf.chars().filter(|&c| c == '\u{FFFD}').count();
        assert_eq!(
            replacement_count, 4,
            "each invalid byte should produce one U+FFFD"
        );
    }
}
