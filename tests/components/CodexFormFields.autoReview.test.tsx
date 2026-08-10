import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { FormProvider, useForm } from "react-hook-form";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CodexFormFields } from "@/components/providers/forms/CodexFormFields";

type AutoReviewMode = "native" | "auto" | "fallback";

const fetchModelsForConfigMock = vi.hoisted(() => vi.fn());

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

vi.mock("@/lib/api/model-fetch", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api/model-fetch")>();
  return {
    ...actual,
    fetchModelsForConfig: fetchModelsForConfigMock,
  };
});

function TestFields({
  mode,
  onModeChange,
  codexApiKey = "",
  codexBaseUrl = "https://example.com/v1",
  onFallbackModelChange = vi.fn(),
}: {
  mode: AutoReviewMode;
  onModeChange: (mode: AutoReviewMode) => void;
  codexApiKey?: string;
  codexBaseUrl?: string;
  onFallbackModelChange?: (model: string) => void;
}) {
  const form = useForm();
  return (
    <FormProvider {...form}>
      <CodexFormFields
        appId="codex"
        codexApiKey={codexApiKey}
        onApiKeyChange={vi.fn()}
        shouldShowApiKeyLink={false}
        websiteUrl=""
        shouldShowSpeedTest={false}
        codexBaseUrl={codexBaseUrl}
        onBaseUrlChange={vi.fn()}
        isFullUrl={false}
        onFullUrlChange={vi.fn()}
        isEndpointModalOpen={false}
        onEndpointModalToggle={vi.fn()}
        autoSelect={false}
        onAutoSelectChange={vi.fn()}
        codexModel="gpt-5.6-sol"
        onModelChange={vi.fn()}
        apiFormat="openai_responses"
        onApiFormatChange={vi.fn()}
        anthropicAuthField="ANTHROPIC_AUTH_TOKEN"
        onAnthropicAuthFieldChange={vi.fn()}
        impersonateClaudeCode={false}
        onImpersonateClaudeCodeChange={vi.fn()}
        maxOutputTokens=""
        onMaxOutputTokensChange={vi.fn()}
        codexChatReasoning={{}}
        onCodexChatReasoningChange={vi.fn()}
        promptCacheRouting="auto"
        onPromptCacheRoutingChange={vi.fn()}
        codexAutoReviewMode={mode}
        onCodexAutoReviewModeChange={onModeChange}
        codexAutoReviewFallbackModel=""
        onCodexAutoReviewFallbackModelChange={onFallbackModelChange}
        catalogModels={[]}
        onCatalogModelsChange={vi.fn()}
        speedTestEndpoints={[]}
        customUserAgent=""
        onCustomUserAgentChange={vi.fn()}
        localProxyHeadersOverride=""
        onLocalProxyHeadersOverrideChange={vi.fn()}
        localProxyBodyOverride=""
        onLocalProxyBodyOverrideChange={vi.fn()}
      />
    </FormProvider>
  );
}

function renderFields(mode: AutoReviewMode, onModeChange = vi.fn()) {
  return render(<TestFields mode={mode} onModeChange={onModeChange} />);
}

describe("CodexFormFields auto-review routing", () => {
  beforeEach(() => {
    fetchModelsForConfigMock.mockReset();
  });

  it("shows three modes and uses the provider model as the fallback placeholder", async () => {
    const onModeChange = vi.fn();
    const { rerender } = renderFields("auto", onModeChange);

    const modes = screen.getAllByRole("radio");
    expect(modes).toHaveLength(3);
    expect(modes[1]).toHaveAttribute("aria-checked", "true");
    expect(modes[0]).toHaveAttribute("data-state", "unselected");
    expect(modes[1]).toHaveAttribute("data-state", "selected");
    expect(modes[2]).toHaveAttribute("data-state", "unselected");
    expect(modes[1]).toHaveClass("bg-blue-100", "text-blue-800");
    expect(
      document.getElementById("codexAutoReviewFallbackModel"),
    ).toHaveAttribute("placeholder", "gpt-5.6-sol");

    await userEvent.click(modes[0]);
    expect(onModeChange).toHaveBeenCalledWith("native");

    rerender(<TestFields mode="native" onModeChange={vi.fn()} />);
    expect(
      document.getElementById("codexAutoReviewFallbackModel"),
    ).not.toBeInTheDocument();
  });

  it("uses a distinct selected color for each reviewer route", () => {
    const { rerender } = renderFields("native");
    let modes = screen.getAllByRole("radio");
    expect(modes[0]).toHaveClass("bg-emerald-100", "text-emerald-800");

    rerender(<TestFields mode="auto" onModeChange={vi.fn()} />);
    modes = screen.getAllByRole("radio");
    expect(modes[1]).toHaveClass("bg-blue-100", "text-blue-800");

    rerender(<TestFields mode="fallback" onModeChange={vi.fn()} />);
    modes = screen.getAllByRole("radio");
    expect(modes[2]).toHaveClass("bg-amber-100", "text-amber-900");
  });

  it("selects the fallback reviewer model from the fetched model list", async () => {
    fetchModelsForConfigMock.mockResolvedValue([
      {
        id: "review-model",
        ownedBy: "Example",
        contextWindow: 128000,
      },
    ]);
    const onFallbackModelChange = vi.fn();
    const user = userEvent.setup();
    render(
      <TestFields
        mode="auto"
        onModeChange={vi.fn()}
        codexApiKey="test-key"
        onFallbackModelChange={onFallbackModelChange}
      />,
    );

    await user.click(screen.getByTitle("providerForm.fetchModels"));
    await waitFor(() =>
      expect(fetchModelsForConfigMock).toHaveBeenCalledOnce(),
    );

    const fallbackInput = document.getElementById(
      "codexAutoReviewFallbackModel",
    );
    expect(fallbackInput).not.toBeNull();
    await user.click(within(fallbackInput!.parentElement!).getByRole("button"));
    await user.click(
      await screen.findByRole("menuitem", { name: "review-model" }),
    );

    expect(onFallbackModelChange).toHaveBeenCalledWith("review-model");
  });

  it("keeps the latest fetch loading when an obsolete request finishes", async () => {
    const first = deferred<unknown[]>();
    const second = deferred<unknown[]>();
    fetchModelsForConfigMock
      .mockImplementationOnce(() => first.promise)
      .mockImplementationOnce(() => second.promise);
    const user = userEvent.setup();
    const { rerender } = render(
      <TestFields
        mode="auto"
        onModeChange={vi.fn()}
        codexApiKey="test-key"
        codexBaseUrl="https://first.example/v1"
      />,
    );

    await user.click(screen.getByTitle("providerForm.fetchModels"));
    expect(screen.getByTitle("providerForm.fetchModels")).toBeDisabled();

    rerender(
      <TestFields
        mode="auto"
        onModeChange={vi.fn()}
        codexApiKey="test-key"
        codexBaseUrl="https://second.example/v1"
      />,
    );
    await waitFor(() =>
      expect(screen.getByTitle("providerForm.fetchModels")).toBeEnabled(),
    );

    await user.click(screen.getByTitle("providerForm.fetchModels"));
    expect(screen.getByTitle("providerForm.fetchModels")).toBeDisabled();

    first.resolve([]);
    await waitFor(() => {
      expect(fetchModelsForConfigMock).toHaveBeenCalledTimes(2);
      expect(screen.getByTitle("providerForm.fetchModels")).toBeDisabled();
    });

    second.resolve([]);
    await waitFor(() =>
      expect(screen.getByTitle("providerForm.fetchModels")).toBeEnabled(),
    );
  });
});
