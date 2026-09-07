//! Credential-free IPC/history types. Never add a raw Provider or credential field here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationProtocol {
    OpenaiChat,
    OpenaiResponses,
    Anthropic,
    Gemini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMode {
    Direct,
    Ccs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Probe {
    Call,
    Stream,
    Tools,
    Structured,
    Image,
    OutputLimit,
    Cache,
    Thinking,
    Signature,
    CrossSignature,
    Comparison,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetInput {
    pub app_id: String,
    pub provider_id: String,
    #[serde(default)]
    pub model: String,
    pub protocol: Option<ValidationProtocol>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrepareRequest {
    pub target: TargetInput,
    pub mode: ValidationMode,
    pub probes: Vec<Probe>,
    pub comparison_target: Option<TargetInput>,
    pub repeat_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetSummary {
    pub app_id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub endpoint: String,
    pub credential_label: String,
    pub model: String,
    pub protocol: ValidationProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationPlan {
    pub id: String,
    pub target: TargetSummary,
    pub mode: ValidationMode,
    pub probes: Vec<Probe>,
    pub max_requests: u32,
    /// Sum of the per-request output limits, not a character-to-token estimate.
    pub max_output_tokens: u32,
    pub max_duration_seconds: u32,
    pub estimated_cost_usd: Option<String>,
    pub warnings: Vec<String>,
    pub expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison_target: Option<TargetSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Passed,
    Failed,
    NotApplicable,
    Inconclusive,
    NotTested,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub label: String,
    pub value: String,
}

impl Evidence {
    pub(super) fn new(label: &str, value: impl ToString) -> Self {
        Self {
            label: label.into(),
            value: value.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub probe: Probe,
    pub status: ProbeStatus,
    pub summary: String,
    pub evidence: Vec<Evidence>,
    pub request_count: u32,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationRun {
    pub id: String,
    pub plan: ValidationPlan,
    pub status: RunStatus,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub results: Vec<ProbeResult>,
}
