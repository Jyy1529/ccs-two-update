import type { ComponentProps, ReactNode } from "react";
import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderForm } from "@/components/providers/forms/ProviderForm";
import type { AppId } from "@/lib/api";
import { server } from "../msw/server";
import { setSettings } from "../msw/state";
import { createTestQueryClient } from "../utils/testQueryClient";

const toastErrorMock = vi.hoisted(() => vi.fn());

vi.mock("sonner", () => ({
  toast: {
    error: (...args: unknown[]) => toastErrorMock(...args),
    success: vi.fn(),
  },
}));

vi.mock("@/components/JsonEditor", () => ({ default: () => null }));
vi.mock("@/components/ConfirmDialog", () => ({
  ConfirmDialog: () => null,
}));
vi.mock("@/components/providers/forms/BasicFormFields", () => ({
  BasicFormFields: () => null,
}));
vi.mock("@/components/providers/forms/ClaudeFormFields", () => ({
  ClaudeFormFields: ({
    advancedOptionsContent,
  }: {
    advancedOptionsContent?: ReactNode;
  }) => <>{advancedOptionsContent}</>,
}));
vi.mock("@/components/providers/forms/CodexFormFields", () => ({
  CodexFormFields: ({
    advancedOptionsContent,
  }: {
    advancedOptionsContent?: ReactNode;
  }) => <>{advancedOptionsContent}</>,
}));
vi.mock("@/components/providers/forms/GeminiFormFields", () => ({
  GeminiFormFields: ({
    advancedOptionsContent,
  }: {
    advancedOptionsContent?: ReactNode;
  }) => <>{advancedOptionsContent}</>,
}));
vi.mock("@/components/providers/forms/OpenCodeFormFields", () => ({
  OpenCodeFormFields: () => null,
}));
vi.mock("@/components/providers/forms/OpenClawFormFields", () => ({
  OpenClawFormFields: () => null,
}));
vi.mock("@/components/providers/forms/HermesFormFields", () => ({
  HermesFormFields: () => null,
}));
vi.mock("@/components/providers/forms/OmoFormFields", () => ({
  OmoFormFields: () => null,
}));
vi.mock("@/components/providers/forms/CodexConfigEditor", () => ({
  default: () => null,
}));
vi.mock("@/components/providers/forms/GeminiConfigEditor", () => ({
  default: () => null,
}));
vi.mock("@/components/providers/forms/CommonConfigEditor", () => ({
  CommonConfigEditor: () => null,
}));
vi.mock("@/components/providers/forms/ClaudeDesktopProviderForm", () => ({
  ClaudeDesktopProviderForm: () => <div>Claude Desktop form</div>,
}));

type ProviderFormInitialData = ComponentProps<
  typeof ProviderForm
>["initialData"];
type RetryAppId = Extract<AppId, "claude" | "codex" | "gemini">;

const retrySettings: Record<RetryAppId, Record<string, unknown>> = {
  claude: {
    env: {
      ANTHROPIC_BASE_URL: "https://claude.example.com",
      ANTHROPIC_AUTH_TOKEN: "sk-test",
    },
  },
  codex: {
    auth: { OPENAI_API_KEY: "sk-test" },
    config: `model_provider = "custom"
model = "gpt-test"

[model_providers.custom]
name = "custom"
base_url = "https://codex.example.com/v1"
wire_api = "responses"
requires_openai_auth = true`,
  },
  gemini: {
    env: {
      GEMINI_API_KEY: "sk-test",
      GOOGLE_GEMINI_BASE_URL: "https://gemini.example.com",
      GEMINI_MODEL: "gemini-test",
    },
    config: {},
  },
};

const createRetryInitialData = (
  appId: RetryAppId,
  category: "custom" | "official" = "custom",
  meta?: NonNullable<ProviderFormInitialData>["meta"],
): ProviderFormInitialData => ({
  name: `${appId} Relay`,
  category,
  settingsConfig: retrySettings[appId],
  meta,
});

const renderProviderForm = (
  appId: AppId,
  initialData: ProviderFormInitialData,
  onSubmit = vi.fn(),
  onRequestAddProvider?: (onCreated: (providerId: string) => void) => void,
) => {
  const view = render(
    <QueryClientProvider client={createTestQueryClient()}>
      <ProviderForm
        appId={appId}
        providerId="provider-id"
        submitLabel="Save"
        onSubmit={onSubmit}
        onCancel={vi.fn()}
        initialData={initialData}
        onRequestAddProvider={onRequestAddProvider}
      />
    </QueryClientProvider>,
  );

  return { ...view, onSubmit };
};

beforeEach(() => {
  setSettings({
    commonConfigConfirmed: true,
    providerFeatureScopes: {
      localProxyRetry: {
        enabled: true,
        apps: ["claude", "codex", "gemini"],
      },
      agentRoleRouting: { enabled: true, apps: ["codex"] },
      autoReviewRouting: { enabled: true, apps: ["codex"] },
    },
  });
  server.use(
    http.post("http://tauri.local/get_common_config_snippet", () =>
      HttpResponse.json(null),
    ),
    http.post("http://tauri.local/auth_get_status", async ({ request }) => {
      const { authProvider } = (await request.json()) as {
        authProvider: string;
      };
      return HttpResponse.json({
        provider: authProvider,
        authenticated: false,
        default_account_id: null,
        accounts: [],
      });
    }),
    http.post("http://tauri.local/get_hermes_live_provider_ids", () =>
      HttpResponse.json([]),
    ),
    http.post("http://tauri.local/get_claude_desktop_default_routes", () =>
      HttpResponse.json([]),
    ),
  );
});

describe("ProviderForm provider retry policy", () => {
  it("hides the retry policy for Gemini when feature scopes are absent", async () => {
    setSettings({ providerFeatureScopes: undefined });
    renderProviderForm("gemini", createRetryInitialData("gemini"));

    await screen.findByRole("button", { name: "Save" });
    expect(
      screen.queryByRole("button", {
        name: "Local proxy automatic retry",
      }),
    ).not.toBeInTheDocument();
  });

  it("forwards the role add-provider callback through advanced config", async () => {
    const user = userEvent.setup();
    const onRequestAddProvider = vi.fn();
    renderProviderForm(
      "codex",
      createRetryInitialData("codex"),
      vi.fn(),
      onRequestAddProvider,
    );

    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Add provider" }));

    expect(onRequestAddProvider).toHaveBeenCalledTimes(1);
  });

  it.each<RetryAppId>(["claude", "codex", "gemini"])(
    "shows the default retry policy for a custom %s provider",
    async (appId) => {
      renderProviderForm(appId, createRetryInitialData(appId));

      expect(
        await screen.findByRole("switch", {
          name: "Enable retries for this Provider",
        }),
      ).not.toBeChecked();
      expect(
        screen.queryByRole("spinbutton", { name: "Additional retries" }),
      ).not.toBeInTheDocument();
      fireEvent.click(
        screen.getByRole("button", {
          name: "Local proxy automatic retry",
        }),
      );
      expect(screen.getByLabelText("Additional retries")).toHaveValue(0);
      expect(screen.getByLabelText("Retry interval (ms)")).toHaveValue(1000);
      expect(screen.getByLabelText("Error message contains")).toHaveValue(
        "We're currently experiencing high demand, which may cause temporary errors.",
      );
    },
  );

  it("saves the complete disabled retry policy by default", async () => {
    const user = userEvent.setup();
    const { onSubmit } = renderProviderForm(
      "claude",
      createRetryInitialData("claude", "official"),
    );

    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].meta.localProxyRetryPolicy).toEqual({
      enabled: false,
      maxRetries: 0,
      retryDelayMs: 1000,
      customMessages: [
        "We're currently experiencing high demand, which may cause temporary errors.",
      ],
      errorTypes: [],
    });
  });

  it.each<RetryAppId>(["claude", "codex", "gemini"])(
    "shows the retry policy for an official %s provider",
    async (appId) => {
      renderProviderForm(appId, createRetryInitialData(appId, "official"));

      expect(
        await screen.findByRole("button", {
          name: "Local proxy automatic retry",
        }),
      ).toHaveAttribute("aria-expanded", "false");
      fireEvent.click(
        screen.getByRole("button", {
          name: "Local proxy automatic retry",
        }),
      );
      expect(screen.getByLabelText("Additional retries")).toBeInTheDocument();
    },
  );

  it("loads an existing retry policy in edit mode", () => {
    renderProviderForm(
      "codex",
      createRetryInitialData("codex", "custom", {
        localProxyRetryPolicy: {
          maxRetries: 3,
          retryDelayMs: 250,
          customMessages: ["temporary capacity"],
          errorTypes: ["overloaded"],
        },
      }),
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "Local proxy automatic retry",
      }),
    );
    expect(screen.getByLabelText("Additional retries")).toHaveValue(3);
    expect(screen.getByLabelText("Retry interval (ms)")).toHaveValue(250);
    expect(screen.getByLabelText("Error message contains")).toHaveValue(
      "temporary capacity",
    );
    expect(screen.getByLabelText("Overloaded (HTTP 503)")).toBeChecked();
    expect(
      screen.getByRole("switch", {
        name: "Enable retries for this Provider",
      }),
    ).toBeChecked();
  });

  it("loads and saves Codex agent role routing metadata", async () => {
    const user = userEvent.setup();
    const { onSubmit } = renderProviderForm(
      "codex",
      createRetryInitialData("codex", "official", {
        codexAgentRoleRouting: {
          enabled: true,
          frontend: {
            providerId: "provider-b",
            upstreamModel: " frontend-pro ",
            model: " gpt-a ",
            reasoningEffort: "high",
          },
          backend: {
            model: " backend-model ",
            reasoningEffort: "medium",
          },
        },
      }),
    );

    expect(screen.getByLabelText("Enable agent role routing")).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].meta.codexAgentRoleRouting).toEqual({
      enabled: true,
      frontend: {
        providerId: "provider-b",
        upstreamModel: "frontend-pro",
        model: "gpt-a",
        reasoningEffort: "high",
      },
      backend: {
        model: "backend-model",
        reasoningEffort: "medium",
      },
    });
  });

  it("keeps agent role routing exclusive to Codex providers", () => {
    renderProviderForm("claude", createRetryInitialData("claude"));

    expect(
      screen.queryByLabelText("Enable agent role routing"),
    ).not.toBeInTheDocument();
  });

  it("normalizes and saves the provider retry policy", async () => {
    const user = userEvent.setup();
    const { onSubmit } = renderProviderForm(
      "claude",
      createRetryInitialData("claude", "official"),
    );

    await user.click(
      screen.getByRole("switch", {
        name: "Enable retries for this Provider",
      }),
    );
    fireEvent.change(screen.getByLabelText("Additional retries"), {
      target: { value: "2" },
    });
    fireEvent.change(screen.getByLabelText("Retry interval (ms)"), {
      target: { value: "500" },
    });
    fireEvent.change(screen.getByLabelText("Error message contains"), {
      target: { value: " Busy \nbusy\n\nOther " },
    });
    await user.click(screen.getByLabelText("Network and timeout errors"));
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].meta.localProxyRetryPolicy).toEqual({
      enabled: true,
      maxRetries: 2,
      retryDelayMs: 500,
      customMessages: ["Busy", "Other"],
      errorTypes: ["network"],
    });
  });

  it("saves an explicitly enabled zero count as unlimited retries", async () => {
    const user = userEvent.setup();
    const { onSubmit } = renderProviderForm(
      "claude",
      createRetryInitialData("claude", "official"),
    );

    await user.click(
      screen.getByRole("switch", {
        name: "Enable retries for this Provider",
      }),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][0].meta.localProxyRetryPolicy).toEqual({
      enabled: true,
      maxRetries: 0,
      retryDelayMs: 1000,
      customMessages: [
        "We're currently experiencing high demand, which may cause temporary errors.",
      ],
      errorTypes: [],
    });
  });

  it("blocks submission when enabled retries have no trigger", async () => {
    const user = userEvent.setup();
    const { onSubmit } = renderProviderForm(
      "claude",
      createRetryInitialData("claude", "official"),
    );

    await user.click(
      screen.getByRole("switch", {
        name: "Enable retries for this Provider",
      }),
    );
    fireEvent.change(screen.getByLabelText("Error message contains"), {
      target: { value: "" },
    });
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(toastErrorMock).toHaveBeenCalledTimes(1));
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it.each<Extract<AppId, "opencode" | "openclaw" | "hermes">>([
    "opencode",
    "openclaw",
    "hermes",
  ])("does not show the retry policy for %s", (appId) => {
    renderProviderForm(appId, {
      name: `${appId} Relay`,
      category: "custom",
      settingsConfig: {},
    });

    expect(
      screen.queryByRole("spinbutton", { name: "Additional retries" }),
    ).not.toBeInTheDocument();
  });

  it("routes Claude Desktop to its dedicated form without retry policy", () => {
    renderProviderForm("claude-desktop", {
      name: "Claude Desktop",
      category: "custom",
      settingsConfig: {},
    });

    expect(screen.getByText("Claude Desktop form")).toBeInTheDocument();
    expect(
      screen.queryByRole("spinbutton", { name: "Additional retries" }),
    ).not.toBeInTheDocument();
  });
});
