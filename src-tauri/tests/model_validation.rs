//! Protocol conformance can compile independently of desktop service registration.
//! These are the production DTO/codec sources, not a replacement transport or DB.

#[path = "../src/services/model_validation/types.rs"]
#[allow(dead_code)]
mod types;
use types::ValidationProtocol;

#[path = "../src/services/model_validation/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use serde_json::json;

#[test]
fn protocol_model_validation_roundtrips_preserve_required_gemini_signatures() {
    let reply = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"ccs_add","args":{"a":19,"b":23}},"thoughtSignature":"synthetic-not-real-signature"}]}}]});
    let call = protocol::synthetic_tool_call(ValidationProtocol::Gemini, &reply).unwrap();
    let mut body = protocol::request(
        ValidationProtocol::Gemini,
        "test-model",
        "synthetic",
        256,
        false,
    );
    protocol::tools(ValidationProtocol::Gemini, &mut body);
    let second = protocol::tool_roundtrip(ValidationProtocol::Gemini, &body, &call);
    assert_eq!(
        second["contents"][1]["parts"][0]["thoughtSignature"],
        "synthetic-not-real-signature"
    );
    assert_eq!(
        second["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
        42
    );
}

#[test]
fn protocol_model_validation_requires_nonempty_thinking_evidence() {
    for (protocol, value) in [
        (
            ValidationProtocol::Anthropic,
            json!({"content":[{"type":"thinking","thinking":""}]}),
        ),
        (
            ValidationProtocol::OpenaiResponses,
            json!({"output":[{"type":"reasoning","summary":[]}]}),
        ),
        (
            ValidationProtocol::Gemini,
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":""}]}}]}),
        ),
    ] {
        assert!(!protocol::observe(protocol, &value).thinking_present);
    }
}

#[test]
fn protocol_model_validation_sse_rejects_heartbeat_only_and_bad_json() {
    let mut parser = protocol::SseDecoder::new(ValidationProtocol::OpenaiChat);
    parser
        .feed(
            b": heartbeat\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            5,
        )
        .unwrap();
    assert_eq!(parser.first_content_ms, None);
    parser.feed(b"data: NOT-JSON\n\n", 10).unwrap();
    parser
        .feed(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            20,
        )
        .unwrap();
    assert!(!parser.complete());
}

#[test]
fn protocol_model_validation_cap_and_storage_parameters_are_explicit() {
    let body = protocol::request(
        ValidationProtocol::OpenaiResponses,
        "test-model",
        "synthetic",
        16,
        false,
    );
    assert_eq!(body["max_output_tokens"], 16);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    let endpoint = protocol::endpoint(ValidationProtocol::Gemini, "a?key=x#y", false);
    assert!(endpoint.contains("%3F"));
    assert!(!endpoint.contains('?'));
    assert!(!endpoint.contains('#'));
}

#[test]
fn protocol_model_validation_rejects_unexpected_tool_args() {
    for arguments in [
        r#"{"a":19,"b":23,"command":"anything"}"#,
        r#"{"a":0,"b":42}"#,
    ] {
        let reply = json!({"choices":[{"message":{"tool_calls":[{"id":"c1","function":{"name":"ccs_add","arguments":arguments}}]}}]});
        assert!(protocol::synthetic_tool_call(ValidationProtocol::OpenaiChat, &reply).is_none());
    }
}
