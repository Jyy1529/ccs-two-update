//! Small protocol-specific synthetic requests and evidence extraction. Raw JSON
//! is retained only in memory for the tool/signature control request.

use super::ValidationProtocol as Protocol;
use serde_json::{json, Value};

pub(super) const BASIC_TOKENS: u32 = 256;
pub(super) const THINKING_TOKENS: u32 = 2048;
pub(super) const LIMIT_TOKENS: u32 = 16;
pub(super) const COMPARISON_TOKENS: u32 = 128;
pub(super) const CALL_PROMPT: &str =
    "This is a synthetic capability check. Reply exactly CCS_OK, with no other text.";

pub(super) fn request(
    protocol: Protocol,
    model: &str,
    prompt: &str,
    tokens: u32,
    stream: bool,
) -> Value {
    match protocol {
        Protocol::OpenaiChat => {
            json!({"model":model,"messages":[{"role":"user","content":prompt}],"max_completion_tokens":tokens,"stream":stream})
        }
        Protocol::OpenaiResponses => {
            json!({"model":model,"input":[{"role":"user","content":prompt}],"max_output_tokens":tokens,"stream":stream,"store":false})
        }
        Protocol::Anthropic => {
            json!({"model":model,"messages":[{"role":"user","content":prompt}],"max_tokens":tokens,"stream":stream})
        }
        Protocol::Gemini => {
            json!({"contents":[{"role":"user","parts":[{"text":prompt}]}],"generationConfig":{"maxOutputTokens":tokens},"stream":stream})
        }
    }
}

pub(super) fn endpoint(protocol: Protocol, model: &str, stream: bool) -> String {
    match protocol {
        Protocol::OpenaiChat => "/v1/chat/completions".into(),
        Protocol::OpenaiResponses => "/responses".into(),
        Protocol::Anthropic => "/v1/messages".into(),
        Protocol::Gemini => {
            // Encode a single path segment; model names never become a new URL or query.
            let model = model.strip_prefix("models/").unwrap_or(model);
            let encoded: String = url::form_urlencoded::byte_serialize(model.as_bytes()).collect();
            if stream {
                format!("/v1beta/models/{encoded}:streamGenerateContent?alt=sse")
            } else {
                format!("/v1beta/models/{encoded}:generateContent")
            }
        }
    }
}

pub(super) fn schema() -> Value {
    json!({"type":"object","properties":{"answer":{"type":"integer"},"label":{"type":"string"}},"required":["answer","label"],"additionalProperties":false})
}

pub(super) fn structured(protocol: Protocol, body: &mut Value) {
    match protocol {
        Protocol::OpenaiChat => {
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"ccs_check","strict":true,"schema":schema()}})
        }
        Protocol::OpenaiResponses => {
            body["text"] = json!({"format":{"type":"json_schema","name":"ccs_check","strict":true,"schema":schema()}})
        }
        Protocol::Anthropic => {
            body["output_config"] = json!({"format":{"type":"json_schema","schema":schema()}})
        }
        Protocol::Gemini => {
            body["generationConfig"]["responseMimeType"] = json!("application/json");
            body["generationConfig"]["responseJsonSchema"] = schema();
        }
    }
}

pub(super) fn thinking(protocol: Protocol, body: &mut Value) {
    match protocol {
        Protocol::OpenaiChat => body["reasoning_effort"] = json!("low"),
        Protocol::OpenaiResponses => body["reasoning"] = json!({"effort":"low","summary":"auto"}),
        Protocol::Anthropic => body["thinking"] = json!({"type":"enabled","budget_tokens":1024}),
        Protocol::Gemini => {
            body["generationConfig"]["thinkingConfig"] =
                json!({"thinkingBudget":1024,"includeThoughts":true})
        }
    }
}

pub(super) fn tools(protocol: Protocol, body: &mut Value) {
    let parameters = json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false});
    let f = json!({"name":"ccs_add","description":"Synthetic addition only. Call with a=19, b=23, then report the returned sum as digits only.","parameters":parameters});
    match protocol {
        Protocol::OpenaiChat => {
            body["tools"] = json!([{"type":"function","function":f}]);
            body["tool_choice"] = json!({"type":"function","function":{"name":"ccs_add"}});
        }
        Protocol::OpenaiResponses => {
            let mut f = f;
            f["type"] = json!("function");
            body["tools"] = json!([f]);
            body["tool_choice"] = json!({"type":"function","name":"ccs_add"});
        }
        Protocol::Anthropic => {
            body["tools"] = json!([{"name":"ccs_add","description":"Synthetic addition only","input_schema":parameters}]);
            body["tool_choice"] = json!({"type":"tool","name":"ccs_add"});
        }
        Protocol::Gemini => {
            body["tools"] = json!([{"functionDeclarations":[f]}]);
            body["toolConfig"] =
                json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["ccs_add"]}});
        }
    }
}

pub(super) fn with_image(protocol: Protocol, body: &mut Value, png: &str) {
    let prompt = "Name the dominant color of this synthetic image. Reply exactly RED, GREEN, or BLUE. Do not guess from the prompt.";
    let uri = format!("data:image/png;base64,{png}");
    match protocol {
        Protocol::OpenaiChat => {
            body["messages"][0]["content"] = json!([{"type":"text","text":prompt},{"type":"image_url","image_url":{"url":uri,"detail":"low"}}])
        }
        Protocol::OpenaiResponses => {
            body["input"][0]["content"] = json!([{"type":"input_text","text":prompt},{"type":"input_image","image_url":uri,"detail":"low"}])
        }
        Protocol::Anthropic => {
            body["messages"][0]["content"] = json!([{"type":"text","text":prompt},{"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}])
        }
        Protocol::Gemini => {
            body["contents"][0]["parts"] =
                json!([{"text":prompt},{"inlineData":{"mimeType":"image/png","data":png}}])
        }
    }
}

pub(super) fn with_cache_prefix(protocol: Protocol, body: &mut Value, prefix: &str) {
    match protocol {
        Protocol::Anthropic => {
            body["system"] =
                json!([{"type":"text","text":prefix,"cache_control":{"type":"ephemeral"}}])
        }
        Protocol::OpenaiChat => {
            body["messages"]
                .as_array_mut()
                .expect("generated request")
                .insert(0, json!({"role":"system","content":prefix}));
        }
        Protocol::OpenaiResponses => body["instructions"] = json!(prefix),
        Protocol::Gemini => body["systemInstruction"] = json!({"parts":[{"text":prefix}]}),
    }
}

pub(super) fn deterministic(protocol: Protocol, body: &mut Value) {
    match protocol {
        Protocol::Gemini => body["generationConfig"]["temperature"] = json!(0),
        _ => body["temperature"] = json!(0),
    }
}

#[derive(Default, Clone)]
pub(super) struct Observation {
    pub text: String,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_created: Option<u64>,
    pub cache_read: Option<u64>,
    pub thinking_present: bool,
    pub signature_present: bool,
    pub failed: bool,
}

fn string(v: &Value, path: &str) -> Option<String> {
    v.pointer(path).and_then(Value::as_str).map(str::to_string)
}
fn number(v: &Value, path: &str) -> Option<u64> {
    v.pointer(path).and_then(Value::as_u64)
}

pub(super) fn observe(protocol: Protocol, v: &Value) -> Observation {
    let mut out = Observation::default();
    out.failed = v.get("error").is_some_and(|e| !e.is_null());
    match protocol {
        Protocol::OpenaiChat => {
            let message = v.pointer("/choices/0/message").unwrap_or(&Value::Null);
            out.text = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into();
            out.thinking_present = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty());
            out.model = string(v, "/model");
            out.stop_reason = string(v, "/choices/0/finish_reason");
            out.input_tokens = number(v, "/usage/prompt_tokens");
            out.output_tokens = number(v, "/usage/completion_tokens");
            out.reasoning_tokens = number(v, "/usage/completion_tokens_details/reasoning_tokens");
            out.cache_read = number(v, "/usage/prompt_tokens_details/cached_tokens");
        }
        Protocol::OpenaiResponses => {
            if let Some(items) = v.get("output").and_then(Value::as_array) {
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        out.thinking_present |= item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.trim().is_empty())
                            || item.get("summary").and_then(Value::as_array).is_some_and(
                                |summary| {
                                    summary.iter().any(|v| {
                                        v.get("text")
                                            .and_then(Value::as_str)
                                            .is_some_and(|s| !s.trim().is_empty())
                                    })
                                },
                            );
                    }
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("output_text") {
                                out.text.push_str(
                                    block.get("text").and_then(Value::as_str).unwrap_or(""),
                                );
                            }
                        }
                    }
                }
            }
            out.model = string(v, "/model");
            out.stop_reason =
                string(v, "/incomplete_details/reason").or_else(|| string(v, "/status"));
            out.failed |= matches!(
                v.get("status").and_then(Value::as_str),
                Some("failed" | "cancelled")
            );
            out.input_tokens = number(v, "/usage/input_tokens");
            out.output_tokens = number(v, "/usage/output_tokens");
            out.reasoning_tokens = number(v, "/usage/output_tokens_details/reasoning_tokens");
            out.cache_read = number(v, "/usage/input_tokens_details/cached_tokens");
        }
        Protocol::Anthropic => {
            if let Some(content) = v.get("content").and_then(Value::as_array) {
                for block in content {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => out
                            .text
                            .push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
                        Some("thinking" | "redacted_thinking") => {
                            out.thinking_present |= ["thinking", "data"].iter().any(|key| {
                                block
                                    .get(key)
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| !s.trim().is_empty())
                            });
                            out.signature_present |= block
                                .get("signature")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.trim().is_empty());
                        }
                        _ => {}
                    }
                }
            }
            out.model = string(v, "/model");
            out.stop_reason = string(v, "/stop_reason");
            out.input_tokens = number(v, "/usage/input_tokens");
            out.output_tokens = number(v, "/usage/output_tokens");
            out.cache_created = number(v, "/usage/cache_creation_input_tokens");
            out.cache_read = number(v, "/usage/cache_read_input_tokens");
        }
        Protocol::Gemini => {
            if let Some(parts) = v
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        out.thinking_present |= part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.trim().is_empty());
                    } else {
                        out.text
                            .push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                    out.signature_present |= part
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty());
                }
            }
            out.model = string(v, "/modelVersion");
            out.stop_reason = string(v, "/candidates/0/finishReason");
            out.input_tokens = number(v, "/usageMetadata/promptTokenCount");
            out.reasoning_tokens = number(v, "/usageMetadata/thoughtsTokenCount");
            out.output_tokens = number(v, "/usageMetadata/candidatesTokenCount")
                .and_then(|n| n.checked_add(out.reasoning_tokens.unwrap_or(0)));
            out.cache_read = number(v, "/usageMetadata/cachedContentTokenCount");
        }
    }
    out
}

pub(super) struct ToolCall {
    pub id: String,
    // Keep provider-required reasoning/signature context in memory, never history.
    assistant: Value,
}

pub(super) fn synthetic_tool_call(protocol: Protocol, value: &Value) -> Option<ToolCall> {
    let (id, name, args) = match protocol {
        Protocol::OpenaiChat => {
            let calls = value.pointer("/choices/0/message/tool_calls")?.as_array()?;
            if calls.len() != 1 {
                return None;
            }
            let c = &calls[0];
            (
                c.get("id")?.as_str()?,
                c.pointer("/function/name")?.as_str()?,
                serde_json::from_str(c.pointer("/function/arguments")?.as_str()?).ok()?,
            )
        }
        Protocol::OpenaiResponses => {
            let calls: Vec<_> = value
                .get("output")?
                .as_array()?
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("function_call"))
                .collect();
            if calls.len() != 1 {
                return None;
            }
            let c = calls[0];
            (
                c.get("call_id")?.as_str()?,
                c.get("name")?.as_str()?,
                serde_json::from_str(c.get("arguments")?.as_str()?).ok()?,
            )
        }
        Protocol::Anthropic => {
            let calls: Vec<_> = value
                .get("content")?
                .as_array()?
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_use"))
                .collect();
            if calls.len() != 1 {
                return None;
            }
            let c = calls[0];
            (
                c.get("id")?.as_str()?,
                c.get("name")?.as_str()?,
                c.get("input")?.clone(),
            )
        }
        Protocol::Gemini => {
            let calls: Vec<_> = value
                .pointer("/candidates/0/content/parts")?
                .as_array()?
                .iter()
                .filter_map(|c| c.get("functionCall"))
                .collect();
            if calls.len() != 1 {
                return None;
            }
            let c = calls[0];
            (
                "ccs_synthetic",
                c.get("name")?.as_str()?,
                c.get("args")?.clone(),
            )
        }
    };
    if name != "ccs_add"
        || id.is_empty()
        || id.len() > 256
        || id.chars().any(char::is_control)
        || args != json!({"a":19,"b":23})
    {
        return None;
    }
    Some(ToolCall {
        id: id.into(),
        assistant: match protocol {
            Protocol::OpenaiChat => value.pointer("/choices/0/message")?.clone(),
            Protocol::OpenaiResponses => value.get("output")?.clone(),
            Protocol::Anthropic => value.get("content")?.clone(),
            Protocol::Gemini => value.pointer("/candidates/0/content")?.clone(),
        },
    })
}

pub(super) fn tool_roundtrip(protocol: Protocol, original: &Value, call: &ToolCall) -> Value {
    let mut body = original.clone();
    match protocol {
        Protocol::OpenaiChat => {
            body["tool_choice"] = json!("none");
            let messages = body["messages"].as_array_mut().expect("generated request");
            let mut assistant = call.assistant.clone();
            assistant["role"] = json!("assistant");
            messages.push(assistant);
            messages.push(json!({"role":"tool","tool_call_id":call.id,"content":"42"}));
        }
        Protocol::OpenaiResponses => {
            body["tool_choice"] = json!("none");
            let input = body["input"].as_array_mut().expect("generated request");
            input.extend(
                call.assistant
                    .as_array()
                    .expect("validated output array")
                    .iter()
                    .cloned(),
            );
            input.push(json!({"type":"function_call_output","call_id":call.id,"output":"42"}));
        }
        Protocol::Anthropic => {
            body.as_object_mut().expect("request").remove("tool_choice");
            let messages = body["messages"].as_array_mut().expect("generated request");
            messages.push(json!({"role":"assistant","content":call.assistant}));
            messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":call.id,"content":"42"}]}));
        }
        Protocol::Gemini => {
            body["toolConfig"] = json!({"functionCallingConfig":{"mode":"NONE"}});
            let content = body["contents"].as_array_mut().expect("generated request");
            let mut assistant = call.assistant.clone();
            assistant["role"] = json!("model");
            content.push(assistant);
            content.push(json!({"role":"user","parts":[{"functionResponse":{"name":"ccs_add","response":{"result":42}}}]}));
        }
    }
    body
}

/// Incremental SSE parser: decode after a complete line so multibyte UTF-8
/// split across network chunks is preserved. Heartbeats and metadata are not content.
pub(super) struct SseDecoder {
    protocol: Protocol,
    buffer: Vec<u8>,
    data: Vec<String>,
    pub observation: Observation,
    pub event_count: u32,
    pub first_content_ms: Option<u64>,
    pub content_events: u32,
    pub terminal: bool,
    pub malformed: bool,
    pub done_marker: bool,
}

impl SseDecoder {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            buffer: Vec::new(),
            data: Vec::new(),
            observation: Observation::default(),
            event_count: 0,
            first_content_ms: None,
            content_events: 0,
            terminal: false,
            malformed: false,
            done_marker: false,
        }
    }

    pub fn feed(&mut self, bytes: &[u8], elapsed_ms: u64) -> Result<(), ()> {
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = std::str::from_utf8(&line)
                .map_err(|_| ())?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                self.dispatch(elapsed_ms)?;
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).into());
            }
            // event, id, retry and comment/heartbeat lines intentionally do not set TTFC.
            if self.data.iter().map(String::len).sum::<usize>() > 131_072 {
                return Err(());
            }
        }
        if self.buffer.len() > 131_072 {
            return Err(());
        }
        Ok(())
    }

    fn dispatch(&mut self, ms: u64) -> Result<(), ()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data).join("\n");
        self.event_count += 1;
        if self.event_count > 5000 {
            return Err(());
        }
        if data.trim() == "[DONE]" {
            self.done_marker = true;
            return Ok(());
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            self.malformed = true;
            return Ok(());
        };
        let event_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "error" || v.get("error").is_some_and(|v| !v.is_null()) {
            self.observation.failed = true;
        }
        let mut text = String::new();
        let mut meaningful = false;
        match self.protocol {
            Protocol::OpenaiChat => {
                text = string(&v, "/choices/0/delta/content").unwrap_or_default();
                let thinking = string(&v, "/choices/0/delta/reasoning_content")
                    .is_some_and(|s| !s.trim().is_empty());
                self.observation.thinking_present |= thinking;
                meaningful = thinking
                    || string(&v, "/choices/0/delta/tool_calls/0/function/arguments")
                        .is_some_and(|s| !s.trim().is_empty());
                let o = observe(self.protocol, &v);
                self.merge(o);
                if string(&v, "/choices/0/finish_reason").is_some() {
                    self.terminal = true;
                }
            }
            Protocol::OpenaiResponses => {
                if event_type == "response.output_text.delta" {
                    text = string(&v, "/delta").unwrap_or_default();
                }
                meaningful = matches!(
                    event_type,
                    "response.function_call_arguments.delta"
                        | "response.reasoning_summary_text.delta"
                        | "response.reasoning_text.delta"
                ) && string(&v, "/delta").is_some_and(|s| !s.trim().is_empty());
                self.observation.thinking_present |= meaningful
                    && matches!(
                        event_type,
                        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta"
                    );
                if matches!(
                    event_type,
                    "response.completed" | "response.incomplete" | "response.failed"
                ) {
                    self.terminal = true;
                    let o = observe(self.protocol, v.get("response").unwrap_or(&Value::Null));
                    self.merge(o);
                    if event_type == "response.failed" {
                        self.observation.failed = true;
                    }
                }
            }
            Protocol::Anthropic => {
                if event_type == "content_block_delta" {
                    text = string(&v, "/delta/text").unwrap_or_default();
                    let thinking =
                        string(&v, "/delta/thinking").is_some_and(|s| !s.trim().is_empty());
                    self.observation.thinking_present |= thinking;
                    meaningful = thinking
                        || string(&v, "/delta/partial_json").is_some_and(|s| !s.trim().is_empty());
                    self.observation.signature_present |=
                        string(&v, "/delta/signature").is_some_and(|s| !s.trim().is_empty());
                }
                if event_type == "content_block_start" {
                    text = string(&v, "/content_block/text").unwrap_or_default();
                }
                if event_type == "message_start" {
                    self.merge(observe(
                        self.protocol,
                        v.get("message").unwrap_or(&Value::Null),
                    ));
                }
                if event_type == "message_delta" {
                    self.observation.stop_reason = string(&v, "/delta/stop_reason");
                    if let Some(tokens) = number(&v, "/usage/output_tokens") {
                        self.observation.output_tokens = Some(tokens);
                    }
                }
                if event_type == "message_stop" {
                    self.terminal = true;
                }
            }
            Protocol::Gemini => {
                let mut o = observe(self.protocol, &v);
                text = std::mem::take(&mut o.text);
                meaningful = o.thinking_present;
                if o.stop_reason.is_some() {
                    self.terminal = true;
                }
                self.merge(o);
            }
        }
        if !text.trim().is_empty() || meaningful {
            self.content_events += 1;
            self.first_content_ms.get_or_insert(ms);
        }
        self.observation.text.push_str(&text);
        Ok(())
    }

    fn merge(&mut self, o: Observation) {
        macro_rules! merge { ($($field:ident),+) => { $(if o.$field.is_some() { self.observation.$field = o.$field; })+ }; }
        merge!(
            model,
            stop_reason,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_created,
            cache_read
        );
        self.observation.failed |= o.failed;
        self.observation.thinking_present |= o.thinking_present;
        self.observation.signature_present |= o.signature_present;
    }

    pub fn complete(&self) -> bool {
        self.buffer.is_empty()
            && self.data.is_empty()
            && !self.malformed
            && self.terminal
            && !self.observation.failed
            && (self.protocol != Protocol::OpenaiChat || self.done_marker)
            && self.observation.stop_reason.is_some()
    }
}
