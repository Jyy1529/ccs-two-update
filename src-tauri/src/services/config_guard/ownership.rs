//! Locally defined ownership. Provider/imported JSON cannot grant write rights.
use crate::app_config::AppType;
use std::{collections::BTreeSet, path::Path};

pub(super) fn defaults(app: &AppType, path: &Path) -> BTreeSet<String> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let fields: &[&str] = match (app, name) {
        (AppType::Codex, "config.toml") => &[
            "/model", "/model_provider", "/model_reasoning_effort", "/model_reasoning_summary", "/model_verbosity",
            "/model_context_window", "/model_auto_compact_token_limit", "/model_catalog_json", "/web_search",
            "/model_providers/*/name", "/model_providers/*/base_url", "/model_providers/*/wire_api",
            "/model_providers/*/env_key", "/model_providers/*/requires_openai_auth", "/model_providers/*/experimental_bearer_token",
            "/model_providers/*/http_headers", "/model_providers/*/env_http_headers", "/model_providers/*/query_params",
            "/mcp_servers/*/type", "/mcp_servers/*/command", "/mcp_servers/*/args", "/mcp_servers/*/env", "/mcp_servers/*/url",
            "/mcp_servers/*/http_headers", "/mcp_servers/*/enabled", "/mcp_servers/*/bearer_token_env_var",
        ],
        (AppType::Codex, "auth.json") => &["/OPENAI_API_KEY"],
        (AppType::Codex, crate::codex_config::CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME) => &[
            "/models/*/slug", "/models/*/display_name", "/models/*/description", "/models/*/context_window",
            "/models/*/default_reasoning_level", "/models/*/supported_reasoning_levels", "/models/*/visibility",
            "/models/*/priority", "/models/*/supported_in_api", "/models/*/shell_type", "/models/*/base_instructions",
            "/models/*/supports_parallel_tool_calls", "/models/*/effective_context_window_percent",
        ],
        (AppType::Claude, _) => &[
            "/env/ANTHROPIC_BASE_URL", "/env/ANTHROPIC_AUTH_TOKEN", "/env/ANTHROPIC_API_KEY", "/env/ANTHROPIC_MODEL",
            "/env/ANTHROPIC_DEFAULT_HAIKU_MODEL", "/env/ANTHROPIC_DEFAULT_SONNET_MODEL", "/env/ANTHROPIC_DEFAULT_OPUS_MODEL",
            "/env/ANTHROPIC_DEFAULT_FABLE_MODEL", "/env/ANTHROPIC_REASONING_MODEL", "/env/CLAUDE_CODE_SUBAGENT_MODEL",
            "/model", "/primaryApiKey", "/mcpServers/*/command", "/mcpServers/*/args", "/mcpServers/*/env",
            "/mcpServers/*/url", "/mcpServers/*/headers", "/mcpServers/*/type",
        ],
        (AppType::ClaudeDesktop, _) => &[
            "/deploymentMode", "/inferenceGatewayApiKey", "/inferenceGatewayAuthScheme", "/inferenceGatewayBaseUrl",
            "/inferenceProvider", "/inferenceModels", "/disableDeploymentModeChooser", "/coworkEgressAllowedHosts",
            "/enterpriseConfig/inferenceGatewayApiKey", "/enterpriseConfig/inferenceGatewayAuthScheme", "/enterpriseConfig/inferenceGatewayBaseUrl",
            "/enterpriseConfig/inferenceProvider", "/enterpriseConfig/disableDeploymentModeChooser", "/appliedId", "/entries/*/id", "/entries/*/name",
        ],
        (AppType::Gemini, ".env") => &["/GEMINI_API_KEY", "/GOOGLE_GEMINI_BASE_URL", "/GEMINI_MODEL"],
        (AppType::Gemini, _) => &["/security/auth/selectedType", "/mcpServers/*/command", "/mcpServers/*/args", "/mcpServers/*/env", "/mcpServers/*/url", "/mcpServers/*/httpUrl", "/mcpServers/*/headers", "/mcpServers/*/type"],
        (AppType::GrokBuild, _) => &["/model/*/api_key", "/model/*/base_url", "/model/*/model", "/model/*/name", "/model/*/api_backend", "/model/*/context_window", "/model/*/env_key", "/default_model", "/mcp_servers"],
        (AppType::OpenCode, _) => &["/provider/*/npm", "/provider/*/name", "/provider/*/options/baseURL", "/provider/*/options/apiKey", "/provider/*/models/*/id", "/provider/*/models/*/name", "/provider/*/models/*/limit", "/model", "/small_model", "/mcp", "/plugin"],
        (AppType::OpenClaw, _) => &["/models/providers/*/baseUrl", "/models/providers/*/apiKey", "/models/providers/*/api", "/models/providers/*/models", "/agents/defaults/model", "/tools", "/env"],
        (AppType::Hermes, _) => &["/model/default", "/model/provider", "/model/base_url", "/providers/*/base_url", "/providers/*/api_key", "/providers/*/model", "/custom_providers/*/name", "/custom_providers/*/base_url", "/custom_providers/*/api_key", "/custom_providers/*/model", "/custom_providers/*/models", "/mcp_servers"],
        (AppType::DeepSeek, "settings.yaml") => &["/llm-deepseek/baseURL", "/llm-deepseek/apiKeyEnv", "/llm-deepseek/models", "/agent-default-model/model", "/agent-default-model/provider"],
        (AppType::DeepSeek, ".credentials.yaml") => &["/DEEPSEEK_API_KEY"],
        (AppType::Pi, "models.json") => &["/providers/*/apiKey", "/providers/*/baseUrl", "/providers/*/api", "/providers/*/name", "/providers/*/models/*/id", "/providers/*/models/*/name", "/providers/*/models/*/contextWindow", "/providers/*/models/*/maxTokens", "/providers/*/headers"],
        _ => &[],
    };
    fields.iter().map(|field| (*field).to_string()).collect()
}
