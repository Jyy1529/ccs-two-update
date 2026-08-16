import type { PropsWithChildren, ReactElement } from "react";
import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { parse as parseToml } from "smol-toml";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  GrokBuildProviderForm,
  grokApiBackendFromApiFormat,
} from "@/components/providers/forms/GrokBuildProviderForm";
import { createTestQueryClient } from "../utils/testQueryClient";
import { setSettings } from "../msw/state";

vi.mock("@/components/JsonEditor", () => ({
  default: ({
    value,
    onChange,
  }: {
    value: string;
    onChange: (value: string) => void;
  }) => (
    <textarea
      aria-label="raw-config"
      value={value}
      onChange={(event) => onChange(event.target.value)}
    />
  ),
}));

function renderWithQueryClient(ui: ReactElement) {
  const queryClient = createTestQueryClient();
  queryClient.setQueryData(["settings"], {
    providerFeatureScopes: {
      localProxyRetry: {
        enabled: true,
        apps: ["claude", "codex", "grokbuild"],
      },
      agentRoleRouting: { enabled: true, apps: ["codex"] },
      autoReviewRouting: { enabled: true, apps: ["codex"] },
    },
  });
  const Wrapper = ({ children }: PropsWithChildren) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
  return render(ui, { wrapper: Wrapper });
}

beforeEach(() => {
  setSettings({
    providerFeatureScopes: {
      localProxyRetry: {
        enabled: true,
        apps: ["claude", "codex", "grokbuild"],
      },
      agentRoleRouting: { enabled: true, apps: ["codex"] },
      autoReviewRouting: { enabled: true, apps: ["codex"] },
    },
  });
});

async function openAdvancedOptions(user: ReturnType<typeof userEvent.setup>) {
  const advancedButton = screen.getByRole("button", {
    name: /Advanced Options|高级选项/,
  });
  if (advancedButton.getAttribute("aria-expanded") !== "true") {
    await user.click(advancedButton);
  }
}

async function openRetryPolicy(user: ReturnType<typeof userEvent.setup>) {
  await openAdvancedOptions(user);
  const retryButton = screen.getByRole("button", {
    name: "Local proxy automatic retry",
  });
  if (retryButton.getAttribute("aria-expanded") !== "true") {
    await user.click(retryButton);
  }
}
describe("GrokBuildProviderForm", () => {
  it("offers curated Grok Build presets and applies one", async () => {
    const user = userEvent.setup();
    const { container } = renderWithQueryClient(
      <GrokBuildProviderForm
        submitLabel="Save"
        onSubmit={() => {}}
        onCancel={() => {}}
      />,
    );

    // 国产官方直连（cn_official）不在 Grok Build 预设列表里
    expect(screen.queryByRole("button", { name: /BytePlus/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /Kimi/ })).toBeNull();

    await user.click(screen.getByRole("button", { name: /PatewayAI/ }));

    const baseUrlInput =
      container.querySelector<HTMLInputElement>("#codexBaseUrl");
    const nameInput =
      container.querySelector<HTMLInputElement>('input[name="name"]');
    expect(baseUrlInput?.value).toBe("https://api.pateway.ai/v1");
    expect(nameInput?.value).toBe("PatewayAI");
  });

  it("keeps the official Grok form minimal and exposes provider retry", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const { container } = renderWithQueryClient(
      <GrokBuildProviderForm
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Grok Official/ }));

    expect(container.querySelector("#grokbuild-profile")).toBeNull();
    expect(screen.queryByLabelText("API Key")).toBeNull();
    expect(
      screen.queryByRole("button", {
        name: "Local proxy automatic retry",
      }),
    ).not.toBeInTheDocument();

    await openAdvancedOptions(user);
    expect(
      screen.getByRole("button", {
        name: "Local proxy automatic retry",
      }),
    ).toHaveAttribute("aria-expanded", "false");

    await user.click(
      screen.getByRole("switch", {
        name: "Enable retries for this Provider",
      }),
    );
    fireEvent.change(screen.getByLabelText("Additional retries"), {
      target: { value: "2" },
    });
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit.mock.calls[0][0]).toMatchObject({
      name: "Grok Official",
      presetCategory: "official",
      meta: {
        localProxyRetryPolicy: {
          enabled: true,
          maxRetries: 2,
        },
      },
    });
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toEqual({
      config: "",
    });
  });

  it("submits a complete config.toml payload with Grok defaults", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const { container } = renderWithQueryClient(
      <GrokBuildProviderForm
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    const nameInput =
      container.querySelector<HTMLInputElement>('input[name="name"]');
    const baseUrlInput =
      container.querySelector<HTMLInputElement>("#codexBaseUrl");
    expect(nameInput).not.toBeNull();
    expect(baseUrlInput).not.toBeNull();

    fireEvent.change(nameInput!, { target: { value: "Example Relay" } });
    fireEvent.change(baseUrlInput!, {
      target: { value: "https://relay.example.com/v1" },
    });
    fireEvent.change(screen.getByLabelText("API Key"), {
      target: { value: "secret-key" },
    });
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSubmit).toHaveBeenCalledTimes(1);
    const submitted = onSubmit.mock.calls[0][0];
    expect(submitted.icon).toBe("");
    const settings = JSON.parse(submitted.settingsConfig);
    const config = parseToml(settings.config) as any;

    expect(config.models.default).toBe("grok-4.5");
    expect(config.model["grok-4.5"]).toEqual({
      model: "grok-4.5",
      base_url: "https://relay.example.com/v1",
      name: "Example Relay",
      api_key: "secret-key",
      api_backend: "responses",
      context_window: 500000,
    });
  });

  it("maps preset API formats into Grok api_backend", async () => {
    // 预设列表已不含 Chat Completions 条目（国产官方直连被移除），
    // chat/messages 映射分支由纯函数覆盖
    expect(grokApiBackendFromApiFormat("openai_chat")).toBe("chat_completions");
    expect(grokApiBackendFromApiFormat("anthropic")).toBe("messages");
    expect(grokApiBackendFromApiFormat("openai_responses")).toBe("responses");

    // 组件级接线用带显式 apiFormat 的 Responses 预设验证
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    renderWithQueryClient(
      <GrokBuildProviderForm
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    await user.click(screen.getByRole("button", { name: /APIKEY\.FUN/ }));
    await user.type(screen.getByLabelText("API Key"), "secret-key");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSubmit).toHaveBeenCalledTimes(1);
    const submitted = onSubmit.mock.calls[0][0];
    const settings = JSON.parse(submitted.settingsConfig);
    const config = parseToml(settings.config) as any;
    expect(submitted.meta.apiFormat).toBe("openai_responses");
    const selected = config.model[config.models.default];
    expect(selected.api_backend).toBe("responses");
    expect(selected.model).toBe("grok-4.5");
    expect(selected.base_url).toBe("https://api.apikey.fun/v1");
  });

  it("renders localized validation feedback for malformed TOML", async () => {
    const onSubmit = vi.fn();
    renderWithQueryClient(
      <GrokBuildProviderForm
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={() => {}}
      />,
    );

    fireEvent.change(screen.getByLabelText("raw-config"), {
      target: { value: "[models" },
    });

    expect(screen.getByText(/Invalid config\.toml:/)).toBeInTheDocument();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("loads edit-mode values and does not resubmit stale custom endpoints", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const config = `[models]
default = "existing-profile"

[model."existing-profile"]
model = "grok-upstream"
base_url = "https://existing.example.com/v1"
name = "Existing Relay"
api_key = "existing-key"
api_backend = "responses"
context_window = 250000
`;
    const { container } = renderWithQueryClient(
      <GrokBuildProviderForm
        providerId="existing-provider"
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={() => {}}
        initialData={{
          name: "Existing Relay",
          settingsConfig: { config },
          meta: {
            custom_endpoints: {
              "https://deleted.example.com/v1": {
                url: "https://deleted.example.com/v1",
                addedAt: 1,
              },
            },
          },
        }}
      />,
    );

    expect(
      container.querySelector<HTMLInputElement>("#grokbuild-profile")?.value,
    ).toBe("existing-profile");
    expect(
      container.querySelector<HTMLInputElement>("#codexBaseUrl")?.value,
    ).toBe("https://existing.example.com/v1");

    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit.mock.calls[0][0].meta.custom_endpoints).toBeUndefined();
  });

  it("loads and saves an independent provider retry policy", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    const config = `[models]
default = "existing-profile"

[model."existing-profile"]
model = "grok-upstream"
base_url = "https://existing.example.com/v1"
name = "Existing Relay"
api_key = "existing-key"
api_backend = "responses"
context_window = 250000
`;
    renderWithQueryClient(
      <GrokBuildProviderForm
        providerId="existing-provider"
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={() => {}}
        initialData={{
          name: "Existing Relay",
          settingsConfig: { config },
          meta: {
            localProxyRetryPolicy: {
              maxRetries: 3,
              retryDelayMs: 250,
              customMessages: ["temporary capacity"],
              errorTypes: ["overloaded"],
            },
          },
        }}
      />,
    );

    await openRetryPolicy(user);
    expect(screen.getByLabelText("Additional retries")).toHaveValue(3);
    expect(screen.getByLabelText("Retry interval (ms)")).toHaveValue(250);
    expect(screen.getByLabelText("Overloaded (HTTP 503)")).toBeChecked();
    expect(
      screen.getByRole("switch", {
        name: "Enable retries for this Provider",
      }),
    ).toBeChecked();

    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit.mock.calls[0][0].meta.localProxyRetryPolicy).toEqual({
      enabled: true,
      maxRetries: 3,
      retryDelayMs: 250,
      customMessages: ["temporary capacity"],
      errorTypes: ["overloaded"],
    });
  });
});
