import type { ComponentProps } from "react";
import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderForm } from "@/components/providers/forms/ProviderForm";
import type { AppId } from "@/lib/api";
import { server } from "../msw/server";
import { setSettings } from "../msw/state";
import { createTestQueryClient } from "../utils/testQueryClient";

vi.mock("@/components/ConfirmDialog", () => ({ ConfirmDialog: () => null }));
vi.mock("@/components/providers/forms/BasicFormFields", () => ({
  BasicFormFields: () => null,
}));
vi.mock("@/components/providers/forms/ProviderPresetSelector", () => ({
  ProviderPresetSelector: () => <div data-testid="preset-selector" />,
}));
vi.mock("@/components/providers/forms/CommonConfigEditor", () => ({
  CommonConfigEditor: () => <div data-testid="common-config" />,
}));
vi.mock("@/components/JsonEditor", () => ({
  default: ({
    value,
    onChange,
  }: {
    value: string;
    onChange: (value: string) => void;
  }) => (
    <textarea
      aria-label="settings-json"
      value={value}
      onChange={(event) => onChange(event.target.value)}
    />
  ),
}));

vi.mock("@/components/providers/forms/ClaudeFormFields", () => ({
  ClaudeFormFields: () => null,
}));
vi.mock("@/components/providers/forms/CodexFormFields", () => ({
  CodexFormFields: () => null,
}));
vi.mock("@/components/providers/forms/GeminiFormFields", () => ({
  GeminiFormFields: () => null,
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
vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn() },
}));

type InitialData = ComponentProps<typeof ProviderForm>["initialData"];

const renderProviderForm = (
  appId: Extract<AppId, "deepseek" | "pi">,
  initialData?: InitialData,
  onSubmit = vi.fn(),
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
      />
    </QueryClientProvider>,
  );
  return { ...view, onSubmit };
};

beforeEach(() => {
  setSettings({
    commonConfigConfirmed: true,
    providerFeatureScopes: {
      localProxyRetry: { enabled: true, apps: ["claude", "codex"] },
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
  );
});

describe("DeepSeek/Pi provider form", () => {
  it.each([
    [
      "deepseek",
      {
        baseUrl: "https://api.deepseek.com",
        apiKey: "",
        model: "deepseek-v4-flash",
      },
    ],
    [
      "pi",
      {
        name: "",
        api: "openai-completions",
        models: [],
      },
    ],
  ] as const)(
    "uses the %s app-specific config instead of Claude env/config",
    async (appId, expectedConfig) => {
      renderProviderForm(appId);

      const editor = await screen.findByRole("textbox", {
        name: "settings-json",
      });
      expect(JSON.parse((editor as HTMLTextAreaElement).value)).toEqual(
        expectedConfig,
      );
      if (appId === "pi") {
        expect(screen.getByTestId("preset-selector")).toBeInTheDocument();
      } else {
        expect(screen.queryByTestId("preset-selector")).not.toBeInTheDocument();
      }
      expect(screen.queryByTestId("common-config")).not.toBeInTheDocument();
    },
  );

  it("preserves edited Pi JSON when submitting", async () => {
    const { onSubmit } = renderProviderForm("pi", {
      name: "Pi provider",
      category: "custom",
      settingsConfig: {
        baseUrl: "https://old.example",
        apiKey: "old",
        model: "old",
      },
    });
    const editor = await screen.findByRole("textbox", {
      name: "settings-json",
    });
    const config = {
      baseUrl: "https://example.test/v1",
      apiKey: "test-key",
      model: "test-model",
      extra: { preserve: true },
    };
    fireEvent.change(editor, { target: { value: JSON.stringify(config) } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(JSON.parse(onSubmit.mock.calls[0][0].settingsConfig)).toEqual(
      config,
    );
  });
});
