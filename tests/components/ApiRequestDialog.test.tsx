import { StrictMode } from "react";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  afterAll,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import i18n from "i18next";
import zh from "@/i18n/locales/zh.json";
import { ApiRequestDialog } from "@/components/providers/ApiRequestDialog";
import type { ApiRequestEvent, ApiResponse } from "@/lib/api/api-request";
import type { Provider } from "@/types";

const mocks = vi.hoisted(() => ({
  execute: vi.fn(),
  cancel: vi.fn(),
  fetch: vi.fn(),
}));
vi.mock("@/lib/api/api-request", () => ({
  executeApiRequest: mocks.execute,
  cancelApiRequest: mocks.cancel,
}));
vi.mock("@/lib/api/model-fetch", () => ({ fetchModelsForConfig: mocks.fetch }));

const provider: Provider = {
  id: "test-provider",
  name: "Test relay",
  settingsConfig: {
    auth: { OPENAI_API_KEY: "provider-secret" },
    config:
      'model_provider = "custom"\nmodel = "model-one"\n[model_providers.custom]\nbase_url = "https://relay.example/v1"\nwire_api = "responses"',
    modelCatalog: { models: [{ id: "model-one" }, { id: "model-two" }] },
  },
};
const result: ApiResponse = {
  status: 200,
  headers: [["content-type", "application/json"]],
  body: '{"output":[{"text":"complete response"}],"usage":{"total_tokens":25}}',
  complete: true,
  error: null,
  durationMs: 42,
};

const originalScroll = Object.getOwnPropertyDescriptor(
  Element.prototype,
  "scrollIntoView",
);
beforeAll(() =>
  Object.defineProperty(Element.prototype, "scrollIntoView", {
    value: vi.fn(),
    configurable: true,
  }),
);
afterAll(() => {
  if (originalScroll)
    Object.defineProperty(Element.prototype, "scrollIntoView", originalScroll);
  else Reflect.deleteProperty(Element.prototype, "scrollIntoView");
});

beforeEach(() => {
  i18n.addResourceBundle(
    "zh",
    "translation",
    { apiRequest: zh.apiRequest },
    true,
    true,
  );
  mocks.execute.mockReset().mockResolvedValue(result);
  mocks.cancel.mockReset().mockResolvedValue(undefined);
  mocks.fetch
    .mockReset()
    .mockResolvedValue([{ id: "fetched-model", ownedBy: "Vendor" }]);
});

function open() {
  const onClose = vi.fn();
  const view = render(
    <StrictMode>
      <ApiRequestDialog provider={provider} appId="codex" onClose={onClose} />
    </StrictMode>,
  );
  return { ...view, onClose };
}

describe("API request debugger", () => {
  it("uses the configured model, default prompt and streaming without sending automatically", () => {
    open();
    expect(screen.getByLabelText("模型")).toHaveValue("model-one");
    expect(screen.getByLabelText("提示词")).toHaveValue(
      "搜索一下今日科技和ai热点",
    );
    expect(screen.getByRole("switch", { name: "流式响应" })).toBeChecked();
    expect(
      screen.getByLabelText("cURL 请求", { selector: "pre" }),
    ).not.toHaveTextContent("provider-secret");
    expect(mocks.execute).not.toHaveBeenCalled();
    expect(mocks.fetch).not.toHaveBeenCalled();
  });

  it("switches endpoint schema and sends the selected model and custom prompt with the provider key", async () => {
    const user = userEvent.setup();
    open();
    await user.selectOptions(
      screen.getByLabelText("接口路径"),
      "chat-completions",
    );
    await user.click(screen.getByRole("button", { name: "Select model" }));
    await user.click(screen.getByRole("option", { name: "model-two" }));
    await user.clear(screen.getByLabelText("提示词"));
    await user.type(screen.getByLabelText("提示词"), "Explain today's AI news");
    await user.click(screen.getByRole("button", { name: "发送请求" }));
    await screen.findByText("HTTP 200");
    const request = mocks.execute.mock.calls[0][1];
    expect(request.url).toBe("https://relay.example/v1/chat/completions");
    expect(request.headers.authorization).toBe("Bearer provider-secret");
    expect(JSON.parse(request.body)).toEqual({
      model: "model-two",
      messages: [{ role: "user", content: "Explain today's AI news" }],
      stream: true,
    });
    expect(
      screen.getByLabelText("完整响应", { selector: "pre" }).textContent,
    ).toBe(result.body);
  });

  it("copies an executable cURL with the real key while the preview stays masked", async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole("button", { name: "复制 cURL" }));
    expect(await navigator.clipboard.readText()).toContain(
      "Bearer provider-secret",
    );
    expect(
      screen.getByLabelText("cURL 请求", { selector: "pre" }),
    ).not.toHaveTextContent("provider-secret");
  });

  it("fetches additional models without losing the configured choices", async () => {
    const user = userEvent.setup();
    open();
    await user.click(
      screen.getByRole("button", { name: "获取供应商模型列表" }),
    );
    await waitFor(() => expect(mocks.fetch).toHaveBeenCalledOnce());
    await user.click(screen.getByRole("button", { name: "Select model" }));
    expect(
      screen.getByRole("option", { name: "fetched-model" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("option", { name: "model-two" }),
    ).toBeInTheDocument();
  });

  it("keeps model discovery on the provider protocol when the test endpoint changes", async () => {
    const user = userEvent.setup();
    open();
    await user.selectOptions(screen.getByLabelText("接口路径"), "gemini");
    await user.click(
      screen.getByRole("button", { name: "获取供应商模型列表" }),
    );
    await waitFor(() => expect(mocks.fetch).toHaveBeenCalledOnce());
    expect(mocks.fetch).toHaveBeenCalledWith(
      "https://relay.example/v1",
      "provider-secret",
      false,
      undefined,
      undefined,
      { apiFormat: "openai-completions", requestHeaders: {} },
    );
    expect(screen.getByLabelText("接口路径")).toHaveValue("gemini");
  });

  it("retains full HTTP error bodies, headers and the request snapshot", async () => {
    const user = userEvent.setup();
    const body =
      '{"error":{"code":"invalid_api_key","message":"all details retained"}}';
    mocks.execute.mockResolvedValue({
      ...result,
      status: 401,
      body,
      headers: [["x-request-id", "req-test"]],
    });
    open();
    await user.click(screen.getByRole("button", { name: "发送请求" }));
    await screen.findByText("HTTP 401");
    expect(
      screen.getByLabelText("完整响应", { selector: "pre" }).textContent,
    ).toBe(body);
    await user.click(screen.getByText("响应头"));
    expect(screen.getByText("x-request-id: req-test")).toBeInTheDocument();
    await user.click(screen.getByText("本次发送的请求"));
    await user.selectOptions(screen.getByLabelText("接口路径"), "messages");
    expect(screen.getByText(/curl --silent/)).toHaveTextContent(
      "/v1/responses",
    );
    await user.click(screen.getByRole("button", { name: "复制完整响应" }));
    expect(await navigator.clipboard.readText()).toBe(body);
  });

  it("decodes split UTF-8 SSE chunks once in StrictMode and cancels without losing received text", async () => {
    const user = userEvent.setup();
    let onEvent!: (event: ApiRequestEvent) => void;
    let reject!: (error: string) => void;
    mocks.execute.mockImplementation((_id, _request, eventHandler) => {
      onEvent = eventHandler;
      return new Promise((_resolve, rejectRequest) => {
        reject = rejectRequest;
      });
    });
    mocks.cancel.mockImplementation(async () => {
      reject("apiRequest.cancelled");
    });
    const { onClose } = open();
    await user.click(screen.getByRole("button", { name: "发送请求" }));
    expect(screen.getByRole("button", { name: "取消请求" })).toBeDisabled();
    act(() => {
      onEvent({ type: "started" });
      onEvent({
        type: "headers",
        status: 200,
        headers: [["content-type", "text/event-stream"]],
      });
    });
    const text = 'data: {"text":"你好 AI"}\n\n';
    const bytes = new TextEncoder().encode(text);
    for (const byte of bytes) {
      act(() => onEvent({ type: "chunk", data: [byte] }));
    }
    expect(
      screen.getByLabelText("完整响应", { selector: "pre" }).textContent,
    ).toBe(text);
    expect(screen.getByRole("button", { name: "关闭请求调试" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "取消请求" }));
    await screen.findByText("已取消请求，保留已接收的内容");
    expect(
      screen.getByLabelText("完整响应", { selector: "pre" }).textContent,
    ).toBe(text);
    expect(mocks.cancel).toHaveBeenCalledWith(mocks.execute.mock.calls[0][0]);
    await user.click(screen.getByRole("button", { name: "关闭请求调试" }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("shows incomplete timeouts and prevents empty prompts from being sent", async () => {
    const user = userEvent.setup();
    mocks.execute.mockResolvedValue({
      ...result,
      complete: false,
      body: "data: partial\n\n",
      error: "apiRequest.timedOut",
    });
    open();
    await user.click(screen.getByRole("button", { name: "发送请求" }));
    await screen.findByText("请求超时，保留已接收的内容");
    expect(
      screen.getByLabelText("完整响应", { selector: "pre" }).textContent,
    ).toBe("data: partial\n\n");
    await user.clear(screen.getByLabelText("提示词"));
    expect(screen.getByRole("button", { name: "发送请求" })).toBeDisabled();
  });
});
