import { describe, expect, it } from "vitest";
import type { Provider } from "@/types";
import {
  buildApiRequest,
  buildCurlCommand,
  DEFAULT_API_PROMPT,
  getRequestProviderConfig,
  maskApiRequest,
  type RequestProviderConfig,
} from "@/utils/apiRequest";

const config: RequestProviderConfig = {
  baseUrl: "https://relay.example/proxy/v1",
  apiKey: "test-secret",
  model: "claude-sonnet-4-6-thinking",
  models: [],
  protocol: "chat-completions",
  headers: {},
  isFullUrl: false,
};
const options = {
  protocol: "chat-completions" as const,
  model: config.model,
  prompt: DEFAULT_API_PROMPT,
  stream: true,
  maxTokens: 4096,
  timeoutSecs: 120,
};

describe("API request construction", () => {
  it("uses the provider key and preserves a gateway prefix without duplicating v1", () => {
    const request = buildApiRequest(config, options);
    expect(request.url).toBe("https://relay.example/proxy/v1/chat/completions");
    expect(request.headers.authorization).toBe("Bearer test-secret");
    expect(JSON.parse(request.body)).toEqual({
      model: "claude-sonnet-4-6-thinking",
      messages: [{ role: "user", content: "搜索一下今日科技和ai热点" }],
      stream: true,
    });
  });

  it("changes a full Chat Completions URL to Responses and uses input", () => {
    const request = buildApiRequest(
      {
        ...config,
        baseUrl: "https://relay.example/proxy/v1/chat/completions?region=cn",
      },
      { ...options, protocol: "responses", stream: false },
    );
    expect(request.url).toBe(
      "https://relay.example/proxy/v1/responses?region=cn",
    );
    expect(JSON.parse(request.body)).toEqual({
      model: config.model,
      input: DEFAULT_API_PROMPT,
      stream: false,
    });
  });

  it("uses Anthropic's required headers and max_tokens", () => {
    const request = buildApiRequest(config, {
      ...options,
      protocol: "messages",
    });
    expect(request.url).toBe("https://relay.example/proxy/v1/messages");
    expect(request.headers["x-api-key"]).toBe("test-secret");
    expect(request.headers["anthropic-version"]).toBe("2023-06-01");
    expect(request.headers.authorization).toBeUndefined();
    expect(JSON.parse(request.body).max_tokens).toBe(4096);
  });

  it.each([true, false])(
    "builds the Gemini route and contents for stream=%s",
    (stream) => {
      const request = buildApiRequest(
        { ...config, baseUrl: "https://relay.example/google/v1beta" },
        { ...options, protocol: "gemini", model: "models/gemini-test", stream },
      );
      expect(request.url).toBe(
        `https://relay.example/google/v1beta/models/gemini-test:${stream ? "streamGenerateContent?alt=sse" : "generateContent"}`,
      );
      expect(request.headers["x-goog-api-key"]).toBe("test-secret");
      expect(JSON.parse(request.body)).toEqual({
        contents: [{ role: "user", parts: [{ text: DEFAULT_API_PROMPT }] }],
      });
    },
  );

  it("replaces a full Gemini model route and removes only its SSE query", () => {
    const request = buildApiRequest(
      {
        ...config,
        baseUrl:
          "https://relay.example/google/v1beta/models/old:streamGenerateContent?alt=sse&region=cn",
      },
      { ...options, protocol: "responses" },
    );
    expect(request.url).toBe(
      "https://relay.example/google/v1/responses?region=cn",
    );
  });

  it("honors an explicit nonstandard full endpoint", () => {
    const request = buildApiRequest(
      {
        ...config,
        baseUrl: "https://relay.example/invoke?version=2",
        isFullUrl: true,
      },
      options,
    );
    expect(request.url).toBe("https://relay.example/invoke?version=2");
  });

  it("keeps the path contract of an explicit full endpoint without v1", () => {
    const request = buildApiRequest(
      {
        ...config,
        baseUrl: "https://relay.example/api/responses",
        isFullUrl: true,
      },
      options,
    );
    expect(request.url).toBe("https://relay.example/api/chat/completions");
  });

  it("preserves custom provider headers and user-agent", () => {
    const request = buildApiRequest(
      {
        ...config,
        headers: { "X-Organization": "team", "User-Agent": "my-client" },
      },
      options,
    );
    expect(request.headers["x-organization"]).toBe("team");
    expect(request.headers["user-agent"]).toBe("my-client");
  });

  it.each([
    "file:///tmp/file",
    "https://user:secret@relay.example/v1",
    "https://relay.example/#fragment",
  ])("rejects an ambiguous or non-HTTP endpoint: %s", (baseUrl) => {
    expect(() => buildApiRequest({ ...config, baseUrl }, options)).toThrow();
  });

  it("does not execute or lose shell metacharacters in generated commands", () => {
    const prompt = "今日 AI: it's $HOME `whoami` $(echo test)\n下一行";
    const request = buildApiRequest(config, { ...options, prompt });
    const bash = buildCurlCommand(request, "bash");
    const powershell = buildCurlCommand(request, "powershell");
    expect(bash).toContain("it'\\''s $HOME `whoami` $(echo test)");
    expect(powershell).toContain("it''s $HOME `whoami` $(echo test)");
    expect(bash).toContain("--no-buffer");
    expect(powershell).toContain("curl.exe");
    expect(JSON.parse(request.body).messages[0].content).toBe(prompt);
  });

  it("masks credentials for display without changing the executable request", () => {
    const request = buildApiRequest(
      {
        ...config,
        baseUrl: "https://relay.example/v1?key=query-secret",
        headers: { "X-Custom-Token": "other-secret" },
      },
      options,
    );
    const masked = maskApiRequest(request);
    const command = buildCurlCommand(masked, "bash");
    expect(command).not.toContain("test-secret");
    expect(command).not.toContain("other-secret");
    expect(command).not.toContain("query-secret");
    expect(request.headers.authorization).toBe("Bearer test-secret");
  });

  it("requires an explicit model, prompt and provider API key", () => {
    expect(() => buildApiRequest(config, { ...options, model: " " })).toThrow();
    expect(() =>
      buildApiRequest(config, { ...options, prompt: " " }),
    ).toThrow();
    expect(() => buildApiRequest({ ...config, apiKey: "" }, options)).toThrow();
  });
});

describe("provider request defaults", () => {
  const provider: Provider = {
    id: "test",
    name: "Test relay",
    settingsConfig: {
      auth: { OPENAI_API_KEY: "provider-key" },
      config:
        'model_provider = "custom"\nmodel = "gpt-test"\n[model_providers.custom]\nbase_url = "https://relay.example/v1"\nwire_api = "responses"',
      modelCatalog: {
        models: [{ id: "catalog-model", displayName: "Catalog model" }],
      },
    },
  };

  it("reads Codex auth, the active provider URL and configured model catalog", () => {
    const result = getRequestProviderConfig(provider, "codex");
    expect(result).toMatchObject({
      apiKey: "provider-key",
      baseUrl: "https://relay.example/v1",
      model: "gpt-test",
      protocol: "responses",
    });
    expect(result.models.map((m) => m.id)).toEqual([
      "gpt-test",
      "catalog-model",
    ]);
  });

  it("reads a Codex provider-scoped key when auth.json has no API key", () => {
    const result = getRequestProviderConfig(
      {
        ...provider,
        settingsConfig: {
          ...provider.settingsConfig,
          auth: {},
          config:
            provider.settingsConfig.config +
            '\nexperimental_bearer_token = "scoped-key"',
        },
      },
      "codex",
    );
    expect(result.apiKey).toBe("scoped-key");
  });

  it("prefers the active Codex provider token over a stale shared auth key", () => {
    const result = getRequestProviderConfig(
      {
        ...provider,
        settingsConfig: {
          ...provider.settingsConfig,
          auth: { OPENAI_API_KEY: "stale-shared-key" },
          config:
            provider.settingsConfig.config +
            '\nexperimental_bearer_token = "scoped-key"\n[model_providers.other]\nexperimental_bearer_token = "other-provider-key"',
        },
      },
      "codex",
    );
    expect(result.apiKey).toBe("scoped-key");
  });

  it("selects a Claude native endpoint and keeps configured model choices", () => {
    const result = getRequestProviderConfig(
      {
        ...provider,
        settingsConfig: {
          env: {
            ANTHROPIC_API_KEY: "claude-key",
            ANTHROPIC_BASE_URL: "https://relay.example/anthropic",
            ANTHROPIC_MODEL: "claude-main",
            ANTHROPIC_DEFAULT_SONNET_MODEL: "claude-sonnet",
          },
        },
      },
      "claude",
    );
    expect(result).toMatchObject({
      apiKey: "claude-key",
      protocol: "messages",
      model: "claude-main",
    });
    expect(result.models.map((m) => m.id)).toEqual([
      "claude-main",
      "claude-sonnet",
    ]);
  });

  it("honors the selected Claude key field and Codex upstream protocol metadata", () => {
    expect(
      getRequestProviderConfig(
        {
          ...provider,
          meta: { apiKeyField: "ANTHROPIC_API_KEY" },
          settingsConfig: {
            env: {
              ANTHROPIC_AUTH_TOKEN: "old-token",
              ANTHROPIC_API_KEY: "selected-key",
            },
          },
        },
        "claude",
      ).apiKey,
    ).toBe("selected-key");
    expect(
      getRequestProviderConfig(
        { ...provider, meta: { apiFormat: "anthropic" } },
        "codex",
      ).protocol,
    ).toBe("messages");
    expect(
      getRequestProviderConfig(
        { ...provider, meta: { apiFormat: "gemini_native" } },
        "codex",
      ).protocol,
    ).toBe("gemini");
  });
});
