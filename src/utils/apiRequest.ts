import { parse as parseToml } from "smol-toml";
import type { AppId } from "@/lib/api";
import type { ApiRequest } from "@/lib/api/api-request";
import type { FetchedModel } from "@/lib/api/model-fetch";
import type { Provider } from "@/types";
import {
  extractCodexBaseUrl,
  extractCodexExperimentalBearerToken,
  extractCodexModelName,
  extractCodexWireApi,
} from "@/utils/providerConfigUtils";

export const DEFAULT_API_PROMPT = "搜索一下今日科技和ai热点";
export const API_REQUEST_APPS: AppId[] = [
  "claude",
  "claude-desktop",
  "codex",
  "gemini",
  "opencode",
  "openclaw",
];
export const API_REQUEST_ENDPOINTS = [
  { id: "chat-completions", label: "Chat Completions · /v1/chat/completions" },
  { id: "responses", label: "Responses · /v1/responses" },
  { id: "messages", label: "Anthropic Messages · /v1/messages" },
  { id: "gemini", label: "Gemini · /v1beta/models/{model}:generateContent" },
] as const;
export type ApiProtocol = (typeof API_REQUEST_ENDPOINTS)[number]["id"];
export type CurlShell = "bash" | "powershell";

export interface RequestProviderConfig {
  baseUrl: string;
  apiKey: string;
  model: string;
  models: FetchedModel[];
  protocol: ApiProtocol;
  headers: Record<string, string>;
  isFullUrl: boolean;
}

export interface ApiRequestOptions {
  protocol: ApiProtocol;
  model: string;
  prompt: string;
  stream: boolean;
  maxTokens: number;
  timeoutSecs: number;
}

const stringValue = (value: unknown): string =>
  typeof value === "string" ? value.trim() : "";

export function getRequestProviderConfig(
  provider: Provider,
  appId: AppId,
): RequestProviderConfig {
  const config = provider.settingsConfig;
  const env = config.env ?? {};
  let baseUrl = "";
  let apiKey = "";
  let model = "";
  let protocol: ApiProtocol = "chat-completions";
  const modelIds: string[] = [];
  let customHeaders: unknown = config.headers;

  if (appId === "codex") {
    const toml = stringValue(config.config);
    baseUrl = extractCodexBaseUrl(toml) ?? "";
    apiKey =
      extractCodexExperimentalBearerToken(toml) ||
      stringValue(config.auth?.OPENAI_API_KEY) ||
      stringValue(env.CODEX_API_KEY) ||
      stringValue(env.OPENAI_API_KEY);
    model = extractCodexModelName(toml) ?? "";
    protocol =
      provider.meta?.apiFormat === "openai_chat" ||
      extractCodexWireApi(toml) === "chat"
        ? "chat-completions"
        : "responses";
    // Parse only the active provider table; another provider's headers must not leak.
    if (toml) {
      const root = parseToml(toml);
      const providers = root.model_providers as
        | Record<string, Record<string, unknown>>
        | undefined;
      customHeaders = providers?.[String(root.model_provider)]?.http_headers;
    }
    if (Array.isArray(config.modelCatalog?.models)) {
      modelIds.push(
        ...config.modelCatalog.models.map((entry: { id?: unknown }) =>
          stringValue(entry.id),
        ),
      );
    }
  } else if (appId === "claude" || appId === "claude-desktop") {
    baseUrl = stringValue(env.ANTHROPIC_BASE_URL);
    apiKey =
      stringValue(env[provider.meta?.apiKeyField ?? "ANTHROPIC_AUTH_TOKEN"]) ||
      stringValue(env.ANTHROPIC_API_KEY) ||
      stringValue(env.OPENROUTER_API_KEY) ||
      stringValue(env.GOOGLE_API_KEY);
    model = stringValue(env.ANTHROPIC_MODEL);
    protocol =
      provider.meta?.apiFormat === "openai_chat"
        ? "chat-completions"
        : provider.meta?.apiFormat === "openai_responses"
          ? "responses"
          : "messages";
    modelIds.push(
      ...[
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
      ].map((key) => stringValue(env[key])),
    );
  } else if (appId === "gemini") {
    baseUrl = stringValue(env.GOOGLE_GEMINI_BASE_URL);
    apiKey = stringValue(env.GEMINI_API_KEY) || stringValue(env.GOOGLE_API_KEY);
    model = stringValue(env.GEMINI_MODEL);
    protocol = "gemini";
  } else if (appId === "opencode") {
    baseUrl = stringValue(config.options?.baseURL);
    apiKey = stringValue(config.options?.apiKey);
    customHeaders = config.options?.headers;
    modelIds.push(...Object.keys(config.models ?? {}));
    if (config.npm === "@ai-sdk/anthropic") protocol = "messages";
    if (config.npm === "@ai-sdk/google") protocol = "gemini";
  } else if (appId === "openclaw") {
    baseUrl = stringValue(config.baseUrl);
    apiKey = stringValue(config.apiKey);
    if (Array.isArray(config.models)) {
      modelIds.push(
        ...config.models.map((entry: { id?: unknown }) =>
          stringValue(entry.id),
        ),
      );
    }
    if (config.api === "anthropic-messages") protocol = "messages";
    if (config.api === "openai-responses") protocol = "responses";
    if (config.api === "google-generative-ai") protocol = "gemini";
  }

  if (provider.meta?.apiFormat) {
    protocol = (
      {
        anthropic: "messages",
        openai_chat: "chat-completions",
        openai_responses: "responses",
        gemini_native: "gemini",
      } as const
    )[provider.meta.apiFormat];
  }
  const headers: Record<string, string> = {};
  if (
    customHeaders &&
    typeof customHeaders === "object" &&
    !Array.isArray(customHeaders)
  ) {
    for (const [name, value] of Object.entries(customHeaders)) {
      if (typeof value === "string") headers[name.toLowerCase()] = value;
    }
  }
  if (provider.meta?.customUserAgent)
    headers["user-agent"] = provider.meta.customUserAgent;
  const models = [...new Set([model, ...modelIds].filter(Boolean))].map(
    (id) => ({ id, ownedBy: null }),
  );
  return {
    baseUrl,
    apiKey,
    model: model || models[0]?.id || "",
    models,
    protocol,
    headers,
    isFullUrl: provider.meta?.isFullUrl ?? false,
  };
}

function requestUrl(
  config: RequestProviderConfig,
  options: ApiRequestOptions,
): string {
  let url: URL;
  try {
    url = new URL(config.baseUrl.trim());
  } catch {
    throw new Error("apiRequest.invalidUrl");
  }
  if (
    !["https:", "http:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.hash
  ) {
    throw new Error("apiRequest.invalidUrl");
  }
  const original = url.pathname.replace(/\/+$/, "");
  let prefix = original.replace(
    /\/(chat\/completions|responses|messages)$/,
    "",
  );
  const isGeminiRoute =
    /\/models\/[^/]+:(streamGenerateContent|generateContent)$/.test(prefix);
  if (isGeminiRoute) {
    prefix = prefix.replace(
      /\/models\/[^/]+:(streamGenerateContent|generateContent)$/,
      "",
    );
    if (url.searchParams.get("alt") === "sse") url.searchParams.delete("alt");
  }
  if (config.isFullUrl && prefix === original && !isGeminiRoute)
    return url.toString();
  // Explicit full endpoints without a version segment must not acquire /v1.
  const useVersion = !config.isFullUrl || /\/v1(?:beta)?$/.test(prefix);
  prefix = prefix.replace(/\/v1(?:beta)?$/, "");
  if (options.protocol === "gemini") {
    const model = encodeURIComponent(
      options.model.trim().replace(/^models\//, ""),
    );
    url.pathname = `${prefix}${useVersion ? "/v1beta" : ""}/models/${model}:${options.stream ? "streamGenerateContent" : "generateContent"}`;
    if (options.stream) url.searchParams.set("alt", "sse");
  } else {
    const path =
      options.protocol === "chat-completions"
        ? "chat/completions"
        : options.protocol;
    url.pathname = `${prefix}${useVersion ? "/v1" : ""}/${path}`;
  }
  return url.toString();
}

export function buildApiRequest(
  config: RequestProviderConfig,
  options: ApiRequestOptions,
): ApiRequest {
  if (!options.model.trim()) throw new Error("apiRequest.missingModel");
  if (!options.prompt.trim()) throw new Error("apiRequest.missingPrompt");
  if (!config.apiKey.trim() || config.apiKey === "PROXY_MANAGED")
    throw new Error("apiRequest.missingKey");
  if (
    !Number.isInteger(options.timeoutSecs) ||
    options.timeoutSecs < 1 ||
    options.timeoutSecs > 600
  )
    throw new Error("apiRequest.invalidTimeout");
  if (
    options.protocol === "messages" &&
    (!Number.isInteger(options.maxTokens) || options.maxTokens < 1)
  )
    throw new Error("apiRequest.invalidMaxTokens");
  const headers: Record<string, string> = {
    "content-type": "application/json",
    accept: options.stream ? "text/event-stream" : "application/json",
    "accept-encoding": "identity",
  };
  if (options.protocol === "messages") {
    headers["x-api-key"] = config.apiKey;
    headers["anthropic-version"] = "2023-06-01";
  } else if (options.protocol === "gemini") {
    headers["x-goog-api-key"] = config.apiKey;
  } else {
    headers.authorization = `Bearer ${config.apiKey}`;
  }
  for (const [name, value] of Object.entries(config.headers))
    headers[name.toLowerCase()] = value;
  const model = options.model.trim();
  const messages = [{ role: "user", content: options.prompt }];
  const body =
    options.protocol === "gemini"
      ? { contents: [{ role: "user", parts: [{ text: options.prompt }] }] }
      : options.protocol === "responses"
        ? { model, input: options.prompt, stream: options.stream }
        : {
            model,
            messages,
            stream: options.stream,
            ...(options.protocol === "messages"
              ? { max_tokens: options.maxTokens }
              : {}),
          };
  return {
    url: requestUrl(config, options),
    headers,
    body: JSON.stringify(body, null, 2),
    timeoutSecs: options.timeoutSecs,
  };
}

export function maskApiRequest(request: ApiRequest): ApiRequest {
  const sensitive = /authorization|api[-_]?key|token|secret|cookie|credential/i;
  const url = new URL(request.url);
  for (const name of [...url.searchParams.keys()]) {
    if (sensitive.test(name) || name.toLowerCase() === "key")
      url.searchParams.set(name, "***");
  }
  return {
    ...request,
    url: url.toString(),
    headers: Object.fromEntries(
      Object.entries(request.headers).map(([name, value]) => [
        name,
        sensitive.test(name) ? "***" : value,
      ]),
    ),
  };
}

/** Single-quoted arguments: prompt text is never interpreted as shell code. */
export function buildCurlCommand(
  request: ApiRequest,
  shell: CurlShell,
): string {
  const quote = (value: string) =>
    `'${value.replace(/'/g, shell === "bash" ? "'\\''" : "''")}'`;
  const lines = [
    `${shell === "bash" ? "curl" : "curl.exe"} --silent --show-error --no-buffer --request POST ${quote(request.url)}`,
    `  --max-time ${request.timeoutSecs}`,
    ...Object.entries(request.headers).map(
      ([name, value]) => `  --header ${quote(`${name}: ${value}`)}`,
    ),
    `  --data-binary ${quote(request.body)}`,
  ];
  return lines.join(shell === "bash" ? " \\\n" : " `\n");
}
