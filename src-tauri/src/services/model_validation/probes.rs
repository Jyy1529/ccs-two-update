//! Semantic assertions and positive/negative controls. A completed run is not
//! an identity certificate, and missing evidence is not silently treated as success.

use super::{
    protocol::{self, BASIC_TOKENS, COMPARISON_TOKENS, LIMIT_TOKENS, THINKING_TOKENS},
    target::{redact, PinnedTarget},
    transport::{elapsed, Executor, Reply, RequestFailure},
    Evidence, Probe, ProbeResult, ProbeStatus, ValidationProtocol,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::io::Write;
use tokio::time::Instant;

pub(super) fn budget(probe: Probe, repeats: u32, comparison: bool) -> (u32, u32) {
    match probe {
        Probe::Tools | Probe::Cache => (2, 2 * BASIC_TOKENS),
        Probe::Thinking => (1, THINKING_TOKENS),
        Probe::Signature => (3, 3 * THINKING_TOKENS),
        Probe::CrossSignature => (5, 5 * THINKING_TOKENS),
        Probe::OutputLimit => (1, LIMIT_TOKENS),
        Probe::Comparison => {
            let requests = repeats * 3 * if comparison { 2 } else { 1 };
            (requests, requests * COMPARISON_TOKENS)
        }
        _ => (1, BASIC_TOKENS),
    }
}

pub(super) async fn run(
    probe: Probe,
    executor: &mut Executor,
    target: &PinnedTarget,
    comparison: Option<&PinnedTarget>,
    repeats: u32,
    nonce: &str,
) -> ProbeResult {
    let started = Instant::now();
    let before = executor.requests;
    let mut result = ProbeResult {
        probe,
        status: ProbeStatus::Inconclusive,
        summary: String::new(),
        evidence: Vec::new(),
        request_count: 0,
        duration_ms: 0,
    };
    let execution = execute(&mut result, executor, target, comparison, repeats, nonce).await;
    if let Err(error) = execution {
        result.status = if error == RequestFailure::Cancelled {
            ProbeStatus::NotTested
        } else {
            ProbeStatus::Inconclusive
        };
        result.summary = error.message().into();
    }
    result.request_count = executor.requests - before;
    result.duration_ms = elapsed(started);
    // Last trust boundary: even a malicious upstream echoing the submitted key
    // cannot put it in history. Signatures/reasoning/raw JSON are never included.
    let mut keys = vec![target.key.as_str()];
    if let Some(other) = comparison {
        keys.push(&other.key);
    }
    result.summary = redact(&result.summary, &keys, 500);
    for item in &mut result.evidence {
        item.label = redact(&item.label, &keys, 100);
        item.value = redact(&item.value, &keys, 600);
    }
    result
}

async fn execute(
    result: &mut ProbeResult,
    e: &mut Executor,
    t: &PinnedTarget,
    comparison: Option<&PinnedTarget>,
    repeats: u32,
    nonce: &str,
) -> Result<(), RequestFailure> {
    let protocol = t.wire_protocol(e.mode);
    if matches!(result.probe, Probe::Signature | Probe::CrossSignature) {
        return signature(result, e, t, comparison).await;
    }
    if result.probe == Probe::Comparison {
        return compare(result, e, t, comparison, repeats).await;
    }
    let tokens = match result.probe {
        Probe::Thinking => THINKING_TOKENS,
        Probe::OutputLimit => LIMIT_TOKENS,
        _ => BASIC_TOKENS,
    };
    let prompt = match result.probe {
        Probe::Tools => "Call ccs_add with a=19 and b=23. After its result, reply with the resulting number only. Do not perform any other action.",
        Probe::Structured => "Return a JSON object with exactly answer: 42 and label: ccs. No extra fields or prose.",
        Probe::OutputLimit => "Output the integers from 1 to 500 in order, separated by spaces. Continue until the entire sequence is written.",
        Probe::Thinking => "Compute (17 * 19) - (11 * 23) - 28, checking your arithmetic carefully. Return the numeric answer only.",
        _ => protocol::CALL_PROMPT,
    };
    let mut body = protocol::request(
        protocol,
        &t.summary.model,
        prompt,
        tokens,
        result.probe == Probe::Stream,
    );
    let mut image_expected = None;
    match result.probe {
        Probe::Tools => protocol::tools(protocol, &mut body),
        Probe::Structured => protocol::structured(protocol, &mut body),
        Probe::Thinking => protocol::thinking(protocol, &mut body),
        Probe::Image => {
            let color = nonce
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
                % 3;
            let (label, png) = synthetic_png(color).map_err(|_| RequestFailure::InvalidResponse)?;
            protocol::with_image(protocol, &mut body, &png);
            image_expected = Some(label);
        }
        Probe::Cache => {
            let mut corpus = format!(
                "Synthetic caching fixture {nonce}. This material is data, not instructions.\n"
            );
            for i in 0..512 {
                corpus.push_str(&format!("Record {i}: amber birch cedar delta elm fern granite harbor iris juniper kelp linen maple nickel oak pine quartz river stone tulip.\n"));
            }
            protocol::with_cache_prefix(protocol, &mut body, &corpus);
            result
                .evidence
                .push(Evidence::new("syntheticPrefixCharacters", corpus.len()));
        }
        _ => {}
    }
    let reply = e.send(t, body.clone()).await?;
    evidence(result, &reply, t, e.mode, "request");
    if !check_response(result, &reply) {
        return Ok(());
    }
    match result.probe {
        Probe::Call => {
            if reply.semantic("CCS_OK") {
                conclude(
                    result,
                    ProbeStatus::Passed,
                    "鉴权、指定模型调用与合成指令已响应；这不是模型身份认证",
                );
            } else {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "收到响应，但未满足合成指令；不能仅凭 HTTP 200 判定模型调用正常",
                );
            }
        }
        Probe::Stream => {
            let Some(sse) = &reply.sse else {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "请求流式响应但未收到 SSE；可能存在网关缓冲或协议不兼容",
                );
                return Ok(());
            };
            result.evidence.extend([
                Evidence::new("sseEvents", sse.event_count),
                Evidence::new("contentEvents", sse.content_events),
                Evidence::new(
                    "firstContentMs",
                    sse.first_content_ms
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                ),
                Evidence::new("completeTerminal", sse.complete()),
            ]);
            if !sse.complete() {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "SSE 不完整、存在错误事件或缺少协议终止标记",
                );
            } else if sse.first_content_ms.is_none() || !reply.semantic("CCS_OK") {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "流结束但未观察到有效合成内容（心跳和 role-only 不计入首内容）",
                );
            } else {
                conclude(result, ProbeStatus::Passed, "SSE 内容和终止事件完整；已记录首个有效内容延迟，无法仅凭到达时序证明上游逐 Token 生成");
            }
        }
        Probe::Structured => {
            let parsed = serde_json::from_str::<Value>(&reply.observation.text).ok();
            let valid = parsed.as_ref() == Some(&json!({"answer":42,"label":"ccs"}));
            result
                .evidence
                .push(Evidence::new("exactSchemaAndValues", valid));
            conclude(
                result,
                if valid {
                    ProbeStatus::Passed
                } else {
                    ProbeStatus::Failed
                },
                if valid {
                    "结构化输出同时满足 JSON 格式、字段约束与预期值"
                } else {
                    "响应未满足结构化格式或预期字段值"
                },
            );
        }
        Probe::Image => {
            let expected = image_expected.unwrap_or("UNKNOWN");
            let valid = reply.observation.text.trim().eq_ignore_ascii_case(expected);
            result
                .evidence
                .push(Evidence::new("syntheticImageExpectedColor", expected));
            result
                .evidence
                .push(Evidence::new("answerMatchesImage", valid));
            conclude(
                result,
                if valid {
                    ProbeStatus::Passed
                } else {
                    ProbeStatus::Failed
                },
                if valid {
                    "识别出未在问题中透露的合成图片主色；仅验证本次图像行为"
                } else {
                    "返回结果与合成图片的真实主色不符"
                },
            );
        }
        Probe::Tools => {
            let Some(call) = protocol::synthetic_tool_call(protocol, &reply.value) else {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "未收到唯一、名称与参数均符合预期的合成工具调用；未执行任何外部工具",
                );
                return Ok(());
            };
            let second = e
                .send(t, protocol::tool_roundtrip(protocol, &body, &call))
                .await?;
            evidence(result, &second, t, e.mode, "roundtrip");
            if !check_response(result, &second) {
                return Ok(());
            }
            let valid = second.semantic("42");
            conclude(
                result,
                if valid {
                    ProbeStatus::Passed
                } else {
                    ProbeStatus::Failed
                },
                if valid {
                    "合成工具的名称、参数和结果往返均符合预期；未执行系统命令"
                } else {
                    "工具调用有效，但后续回复没有正确使用固定的合成工具结果"
                },
            );
        }
        Probe::OutputLimit => {
            result
                .evidence
                .push(Evidence::new("requestedOutputTokenLimit", LIMIT_TOKENS));
            let Some(actual) = reply.observation.output_tokens else {
                conclude(
                    result,
                    ProbeStatus::Inconclusive,
                    "没有可用的输出 Token 计数；不能用字符数代替 Token 判定限制",
                );
                return Ok(());
            };
            if actual > LIMIT_TOKENS as u64 {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "返回的输出 Token 计数超过请求上限",
                );
            } else if matches!(
                reply.observation.stop_reason.as_deref(),
                Some("length" | "max_tokens" | "max_output_tokens" | "MAX_TOKENS")
            ) {
                conclude(
                    result,
                    ProbeStatus::Passed,
                    "Token 计数未超限，且结束原因明确表示触及输出上限",
                );
            } else {
                conclude(
                    result,
                    ProbeStatus::Inconclusive,
                    "输出未超限，但没有触及上限的结束证据；可能提前结束",
                );
            }
        }
        Probe::Cache => {
            let second = e.send(t, body).await?;
            evidence(result, &second, t, e.mode, "repeat");
            if !check_response(result, &second) {
                return Ok(());
            }
            if second.observation.cache_read.is_some_and(|n| n > 0) {
                if protocol == ValidationProtocol::Anthropic
                    && !reply.observation.cache_created.is_some_and(|n| n > 0)
                {
                    conclude(result, ProbeStatus::Inconclusive, "观察到缓存读取计数，但正常首请求缺少缓存创建证据；不宣称创建与读取整套通过");
                } else {
                    conclude(
                        result,
                        ProbeStatus::Passed,
                        if protocol == ValidationProtocol::Anthropic {
                            "首请求缓存创建与重复请求读取均有计数证据"
                        } else {
                            "重复请求有缓存命中计数；该协议未提供独立的创建计数"
                        },
                    );
                }
            } else {
                conclude(
                    result,
                    ProbeStatus::Inconclusive,
                    "未观察到正的缓存命中计数；缓存阈值、建立时延或协议差异仍可能影响结果",
                );
            }
        }
        Probe::Thinking => {
            if !reply.semantic("42") {
                conclude(
                    result,
                    ProbeStatus::Failed,
                    "Thinking 请求有响应，但合成算术结果不符合预期",
                );
            } else if reply.observation.reasoning_tokens.is_some_and(|n| n > 0)
                || reply.observation.thinking_present
            {
                conclude(
                    result,
                    ProbeStatus::Passed,
                    "观察到 Thinking 内容类型或推理 Token 计数，且合成结果正确；不保存推理文本",
                );
            } else {
                conclude(
                    result,
                    ProbeStatus::Inconclusive,
                    "参数被接受但未返回 Thinking 内容类型或正的推理计数；不能据此宣称启用成功",
                );
            }
        }
        _ => unreachable!("advanced probes handled above"),
    }
    Ok(())
}

fn check_response(result: &mut ProbeResult, reply: &Reply) -> bool {
    if reply.ok() {
        return true;
    }
    conclude(
        result,
        if reply.unsupported_parameter() {
            ProbeStatus::NotApplicable
        } else {
            ProbeStatus::Failed
        },
        if reply.unsupported_parameter() {
            "该模型或端点明确不支持本探针参数；未更换参数重试"
        } else {
            "请求失败或返回协议错误；原始错误正文未写入历史"
        },
    );
    false
}

fn conclude(result: &mut ProbeResult, status: ProbeStatus, summary: &str) {
    result.status = status;
    result.summary = summary.into();
}

fn evidence(
    result: &mut ProbeResult,
    reply: &Reply,
    target: &PinnedTarget,
    mode: super::ValidationMode,
    label: &str,
) {
    let push = |items: &mut Vec<Evidence>, key: &str, value: String| {
        items.push(Evidence::new(&format!("{label}.{key}"), value))
    };
    let items = &mut result.evidence;
    push(items, "httpStatus", reply.status.to_string());
    push(items, "durationMs", reply.elapsed_ms.to_string());
    push(items, "requestedModel", target.summary.model.clone());
    push(
        items,
        "upstreamModel",
        if mode == super::ValidationMode::Direct {
            target.summary.model.clone()
        } else {
            reply
                .upstream_model
                .clone()
                .unwrap_or_else(|| "unknown (ccs did not expose transformed model)".into())
        },
    );
    push(
        items,
        "responseDeclaredModel",
        reply
            .observation
            .model
            .clone()
            .unwrap_or_else(|| "unknown".into()),
    );
    if let Some(stop) = &reply.observation.stop_reason {
        push(items, "finishReason", stop.clone());
    }
    for (name, count) in [
        ("inputTokens", reply.observation.input_tokens),
        ("outputTokens", reply.observation.output_tokens),
        ("reasoningTokens", reply.observation.reasoning_tokens),
        ("cacheCreationTokens", reply.observation.cache_created),
        ("cacheReadTokens", reply.observation.cache_read),
    ] {
        push(
            items,
            name,
            count
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".into()),
        );
    }
    push(
        items,
        "thinkingTypeObserved",
        reply.observation.thinking_present.to_string(),
    );
    push(
        items,
        "signatureObserved",
        reply.observation.signature_present.to_string(),
    );
    for hint in &reply.channel_hints {
        push(items, "channelClue", hint.to_string());
    }
}

async fn signature(
    result: &mut ProbeResult,
    e: &mut Executor,
    target: &PinnedTarget,
    other: Option<&PinnedTarget>,
) -> Result<(), RequestFailure> {
    let native = |t: &PinnedTarget| {
        t.wire_protocol(e.mode) == ValidationProtocol::Anthropic
            && t.summary.protocol == ValidationProtocol::Anthropic
    };
    if !native(target) || result.probe == Probe::CrossSignature && !other.is_some_and(native) {
        conclude(
            result,
            ProbeStatus::NotApplicable,
            "签名对照仅适用于保留原生 Anthropic Thinking 签名的协议链路",
        );
        return Ok(());
    }
    let initial = signature_request(target);
    let first = e.send(target, initial.clone()).await?;
    evidence(result, &first, target, e.mode, "seed");
    let Some(content) = signed_content(&first) else {
        conclude(
            result,
            ProbeStatus::Inconclusive,
            "正常请求没有成功返回可用于往返的 Thinking 签名；未发送负向请求",
        );
        return Ok(());
    };
    let control_body = signed_roundtrip(&initial, content.clone());
    let control = e.send(target, control_body.clone()).await?;
    evidence(result, &control, target, e.mode, "normalControl");
    if !control.semantic("CCS_OK") {
        conclude(
            result,
            ProbeStatus::Inconclusive,
            "原签名正常对照没有成功完成；不能将任何后续拒绝计为验签通过",
        );
        return Ok(());
    }
    if result.probe == Probe::Signature {
        let Some(tampered) = tamper_signature(content) else {
            conclude(
                result,
                ProbeStatus::Inconclusive,
                "签名格式不能构造等结构负向对照",
            );
            return Ok(());
        };
        let negative = e.send(target, signed_roundtrip(&initial, tampered)).await?;
        evidence(result, &negative, target, e.mode, "tamperedControl");
        if negative.signature_rejection() {
            conclude(
                result,
                ProbeStatus::Passed,
                "原签名正常对照成功，且等结构篡改请求被明确以签名错误拒绝；不认证具体模型身份",
            );
        } else if negative.ok() {
            conclude(
                result,
                ProbeStatus::Failed,
                "原签名对照成功，但篡改后的签名仍被接受；可能存在签名移除或未校验行为",
            );
        } else {
            conclude(
                result,
                ProbeStatus::Inconclusive,
                "负向请求失败，但错误没有明确指向签名；普通 HTTP 400、鉴权失败或限流不算通过",
            );
        }
    } else {
        let Some(other) = other else {
            conclude(
                result,
                ProbeStatus::NotApplicable,
                "需要用户明确指定第二个供应商",
            );
            return Ok(());
        };
        let own = signature_request(other);
        let second_seed = e.send(other, own.clone()).await?;
        evidence(result, &second_seed, other, e.mode, "comparisonSeed");
        let Some(other_content) = signed_content(&second_seed) else {
            conclude(
                result,
                ProbeStatus::Inconclusive,
                "第二供应商没有可用签名，无法建立其正常对照",
            );
            return Ok(());
        };
        let second_control = e.send(other, signed_roundtrip(&own, other_content)).await?;
        evidence(
            result,
            &second_control,
            other,
            e.mode,
            "comparisonNormalControl",
        );
        if !second_control.semantic("CCS_OK") {
            conclude(
                result,
                ProbeStatus::Inconclusive,
                "第二供应商正常对照失败；不进行跨供应商结论推断",
            );
            return Ok(());
        }
        let crossed = e.send(other, signed_roundtrip(&own, content)).await?;
        evidence(result, &crossed, other, e.mode, "crossedSignature");
        if crossed.semantic("CCS_OK") {
            conclude(
                result,
                ProbeStatus::Passed,
                "两端正常对照成功，跨供应商签名可往返；仅证明本次兼容性，不证明相同来源或型号",
            );
        } else if crossed.signature_rejection() {
            conclude(
                result,
                ProbeStatus::Failed,
                "两端正常对照成功，但跨供应商签名被明确拒绝；仅说明本次互操作不成立",
            );
        } else {
            conclude(
                result,
                ProbeStatus::Inconclusive,
                "跨供应商请求未成功且缺少签名相关错误，无法判断互操作性",
            );
        }
    }
    Ok(())
}

fn signature_request(t: &PinnedTarget) -> Value {
    let mut body = protocol::request(
        ValidationProtocol::Anthropic,
        &t.summary.model,
        "Check carefully: (17 * 19) - (11 * 23) - 28. Reply with the number only.",
        THINKING_TOKENS,
        false,
    );
    protocol::thinking(ValidationProtocol::Anthropic, &mut body);
    body
}

fn signed_content(reply: &Reply) -> Option<Value> {
    if !reply.semantic("42") {
        return None;
    }
    let content = reply.value.get("content")?.as_array()?;
    if content.len() > 16
        || !content.iter().all(|b| {
            matches!(
                b.get("type").and_then(Value::as_str),
                Some("text" | "thinking")
            )
        })
    {
        return None;
    }
    let valid = content.iter().any(|b| {
        b.get("type").and_then(Value::as_str) == Some("thinking")
            && b.get("thinking")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && b.get("signature")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s.len() < 65536)
    });
    valid.then(|| Value::Array(content.clone()))
}

fn signed_roundtrip(initial: &Value, content: Value) -> Value {
    let mut body = initial.clone();
    let messages = body["messages"].as_array_mut().expect("generated request");
    messages.push(json!({"role":"assistant","content":content}));
    messages.push(json!({"role":"user","content":"Reply exactly CCS_OK with no other text."}));
    body
}

fn tamper_signature(mut content: Value) -> Option<Value> {
    for block in content.as_array_mut()? {
        if block.get("type").and_then(Value::as_str) != Some("thinking") {
            continue;
        }
        let signature = block.get("signature")?.as_str()?;
        if !signature.is_ascii() || signature.len() < 2 {
            return None;
        }
        let mut bytes = signature.as_bytes().to_vec();
        let index = bytes.len() / 2;
        bytes[index] = if bytes[index] == b'A' { b'B' } else { b'A' };
        block["signature"] = Value::String(String::from_utf8(bytes).ok()?);
        return Some(content);
    }
    None
}

async fn compare(
    result: &mut ProbeResult,
    e: &mut Executor,
    target: &PinnedTarget,
    other: Option<&PinnedTarget>,
    repeats: u32,
) -> Result<(), RequestFailure> {
    let tasks = [
        ("arithmetic", "Compute 17 * 19. Reply with the integer only.", "323"),
        ("ordering", "Sort these integers ascending: 9, -2, 4, 0. Reply exactly as comma-separated integers with no spaces.", "-2,0,4,9"),
        ("extraction", "Synthetic record: north=pebble; east=amber; south=fern; west=linen. Return the value of east only.", "amber"),
    ];
    let mut correct = [0u32; 2];
    let mut valid = 0u32;
    let targets: Vec<&PinnedTarget> = std::iter::once(target).chain(other).collect();
    // Alternate A/B order between repetitions to reduce order/time effects.
    for repeat in 0..repeats {
        for (task, prompt, expected) in tasks {
            let order: Vec<usize> = if repeat % 2 == 0 {
                (0..targets.len()).collect()
            } else {
                (0..targets.len()).rev().collect()
            };
            for index in order {
                let t = targets[index];
                let p = t.wire_protocol(e.mode);
                let mut body =
                    protocol::request(p, &t.summary.model, prompt, COMPARISON_TOKENS, false);
                protocol::deterministic(p, &mut body);
                let response = e.send(t, body).await?;
                if response.ok() && !response.observation.text.trim().is_empty() {
                    valid += 1;
                }
                let matched = response.semantic(expected);
                correct[index] += u32::from(matched);
                result.evidence.push(Evidence::new(&format!("sample.{}.{}.{}", index + 1, repeat + 1, task),
                    format!("status={}; expected={expected}; answer={}; matched={matched}; elapsedMs={}; upstreamModel={}; responseModel={}",
                        response.status, redact(&response.observation.text, &[&target.key, other.map(|t| t.key.as_str()).unwrap_or("")], 100),
                        response.elapsed_ms,
                        if e.mode == super::ValidationMode::Direct { t.summary.model.as_str() } else { response.upstream_model.as_deref().unwrap_or("unknown") },
                        response.observation.model.as_deref().unwrap_or("unknown"))));
            }
        }
    }
    let total = repeats * tasks.len() as u32;
    result.evidence.push(Evidence::new("controls", format!("3 fixed synthetic tasks; repetitions={repeats}; temperature=0; perRequestOutputTokens={COMPARISON_TOKENS}; interleaved order; no retries")));
    for (index, t) in targets.iter().enumerate() {
        result.evidence.push(Evidence::new(
            &format!("target{}.score", index + 1),
            format!(
                "provider={}; model={}; exactMatches={}/{total}",
                t.summary.provider_name, t.summary.model, correct[index]
            ),
        ));
    }
    conclude(result,
        if valid != total * targets.len() as u32 { ProbeStatus::Inconclusive }
        else if correct.iter().take(targets.len()).all(|n| *n == total) { ProbeStatus::Passed } else { ProbeStatus::Failed },
        "已完成受控重复任务，展示样本正确率与耗时；温度参数支持性、随机性和渠道特殊路由均影响解释，不能据此认定降智或替换模型");
    Ok(())
}

/// Generate a valid 32x32 RGB PNG entirely in memory (no image/runtime dependency).
fn synthetic_png(color: u32) -> Result<(&'static str, String), std::io::Error> {
    let (label, rgb) = match color {
        0 => ("RED", [255u8, 0, 0]),
        1 => ("GREEN", [0u8, 255, 0]),
        _ => ("BLUE", [0u8, 0, 255]),
    };
    let mut pixels = Vec::with_capacity(3104);
    for _ in 0..32 {
        pixels.push(0);
        for _ in 0..32 {
            pixels.extend_from_slice(&rgb);
        }
    }
    let mut compressed =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    compressed.write_all(&pixels)?;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    fn chunk(png: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(tag);
        png.extend_from_slice(data);
        let mut crc = !0u32;
        for byte in tag.iter().chain(data) {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320u32 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        png.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&32u32.to_be_bytes());
    ihdr.extend_from_slice(&32u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &compressed.finish()?);
    chunk(&mut png, b"IEND", &[]);
    Ok((label, STANDARD.encode(png)))
}
