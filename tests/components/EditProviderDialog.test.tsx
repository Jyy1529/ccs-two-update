import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Provider } from "@/types";

const toastMocks = vi.hoisted(() => ({
  success: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => ({ toast: toastMocks }));

const apiMocks = vi.hoisted(() => ({
  getCurrent: vi.fn(),
  getLiveProviderSettings: vi.fn(),
  getOpenClawLiveProvider: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  providersApi: {
    getCurrent: apiMocks.getCurrent,
  },
  vscodeApi: {
    getLiveProviderSettings: apiMocks.getLiveProviderSettings,
  },
  openclawApi: {
    getLiveProvider: apiMocks.getOpenClawLiveProvider,
  },
}));

vi.mock("@/components/providers/forms/ProviderForm", () => ({
  ProviderForm: ({
    initialData,
    onSubmit,
    isProxyTakeover,
    formId = "provider-form",
    onRequestAddCodexProvider,
  }: {
    initialData?: {
      name?: string;
      websiteUrl?: string;
      notes?: string;
      settingsConfig?: Record<string, unknown>;
      meta?: Record<string, unknown>;
      icon?: string;
      iconColor?: string;
    };
    onSubmit: (values: {
      name: string;
      websiteUrl: string;
      notes?: string;
      settingsConfig: string;
      meta?: Record<string, unknown>;
      icon?: string;
      iconColor?: string;
    }) => Promise<void> | void;
    isProxyTakeover?: boolean;
    formId?: string;
    onRequestAddCodexProvider?: (
      onCreated: (providerId: string) => void,
    ) => void;
  }) => {
    const data = initialData ?? {};
    return (
      <div>
        <form
          id={formId}
          onSubmit={(event) => {
            event.preventDefault();
            void Promise.resolve(
              onSubmit({
                name: data.name ?? "",
                websiteUrl: data.websiteUrl ?? "",
                notes: data.notes,
                settingsConfig: JSON.stringify(data.settingsConfig ?? {}),
                meta: data.meta,
                icon: data.icon,
                iconColor: data.iconColor,
              }),
            ).catch(() => undefined);
          }}
        >
          <input
            aria-label={`${formId}-draft`}
            defaultValue={data.name ?? ""}
          />
          <output data-testid="settings-config">
            {JSON.stringify(data.settingsConfig ?? {})}
          </output>
          <output data-testid="is-proxy-takeover">
            {isProxyTakeover ? "true" : "false"}
          </output>
        </form>
        {onRequestAddCodexProvider && (
          <button
            type="button"
            onClick={() => onRequestAddCodexProvider(vi.fn())}
          >
            request role provider
          </button>
        )}
      </div>
    );
  },
}));

import { EditProviderDialog } from "@/components/providers/EditProviderDialog";

describe("EditProviderDialog", () => {
  beforeEach(() => {
    apiMocks.getCurrent.mockReset();
    apiMocks.getLiveProviderSettings.mockReset();
    apiMocks.getOpenClawLiveProvider.mockReset();
    Object.values(toastMocks).forEach((mock) => mock.mockReset());
  });

  it("保留 Codex 数据库中的 modelCatalog，避免 live 配置缺字段时清空模型映射", async () => {
    const dbModelCatalog = {
      models: [
        {
          model: "deepseek-v4-flash",
          displayName: "DeepSeek V4 Flash",
          contextWindow: 1000000,
        },
      ],
    };
    const provider: Provider = {
      id: "deepseek",
      name: "DeepSeek",
      category: "aggregator",
      settingsConfig: {
        auth: {
          OPENAI_API_KEY: "db-key",
        },
        config: 'model_provider = "custom"\nmodel = "deepseek-v4-flash"\n',
        modelCatalog: dbModelCatalog,
      },
    };
    const liveSettings = {
      auth: {
        OPENAI_API_KEY: "live-key",
      },
      config: 'model_provider = "custom"\nmodel = "deepseek-v4-pro"\n',
    };
    const handleSubmit = vi.fn().mockResolvedValue(undefined);

    apiMocks.getCurrent.mockResolvedValue(provider.id);
    apiMocks.getLiveProviderSettings.mockResolvedValue(liveSettings);

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={vi.fn()}
        onSubmit={handleSubmit}
        appId="codex"
      />,
    );

    await waitFor(() => {
      expect(
        JSON.parse(screen.getByTestId("settings-config").textContent ?? "{}"),
      ).toEqual({
        ...liveSettings,
        modelCatalog: dbModelCatalog,
      });
    });

    fireEvent.click(screen.getByRole("button", { name: "common.save" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
    expect(handleSubmit.mock.calls[0][0].provider.settingsConfig).toEqual({
      ...liveSettings,
      modelCatalog: dbModelCatalog,
    });
  });

  it("代理接管中编辑 Codex 供应商时展示数据库配置而不是读取 live 代理配置", async () => {
    const provider: Provider = {
      id: "deepseek",
      name: "DeepSeek",
      category: "custom",
      settingsConfig: {
        auth: {
          OPENAI_API_KEY: "db-key",
        },
        config:
          'model_provider = "custom"\n[model_providers.custom]\nbase_url = "https://api.deepseek.com/v1"\n',
      },
    };

    apiMocks.getCurrent.mockResolvedValue(provider.id);
    apiMocks.getLiveProviderSettings.mockResolvedValue({
      auth: {
        OPENAI_API_KEY: "PROXY_MANAGED",
      },
      config:
        'model_provider = "custom"\n[model_providers.custom]\nbase_url = "http://127.0.0.1:15721/v1"\nexperimental_bearer_token = "PROXY_MANAGED"\n',
    });

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        appId="codex"
        isProxyTakeover
      />,
    );

    await waitFor(() => {
      expect(screen.getByTestId("is-proxy-takeover").textContent).toBe("true");
    });

    expect(apiMocks.getLiveProviderSettings).not.toHaveBeenCalled();
    expect(
      JSON.parse(screen.getByTestId("settings-config").textContent ?? "{}"),
    ).toEqual(provider.settingsConfig);
  });

  it("成功更新当前启用角色路由的 Codex Provider 后显示代理与重启提示", async () => {
    const provider: Provider = {
      id: "provider-a",
      name: "Provider A",
      settingsConfig: {},
      meta: {
        codexAgentRoleRouting: {
          enabled: true,
          frontend: {},
          backend: {},
        },
      },
    };
    const handleSubmit = vi.fn().mockResolvedValue(undefined);
    const handleOpenChange = vi.fn();
    apiMocks.getCurrent.mockResolvedValue(provider.id);

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={handleOpenChange}
        onSubmit={handleSubmit}
        appId="codex"
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.save" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
    expect(toastMocks.success).toHaveBeenCalledWith(
      "providerAdvanced.agentRoleProxyAutoEnabled",
    );
    expect(toastMocks.info).toHaveBeenCalledWith(
      "providerAdvanced.agentRoleRestartRequired",
    );
    expect(toastMocks.info).not.toHaveBeenCalledWith(
      "providerAdvanced.agentRoleSavedSwitchRequired",
    );
    expect(handleOpenChange).toHaveBeenCalledWith(false);
  });

  it("成功更新非当前角色路由 Provider 后提示切换再生效", async () => {
    const provider: Provider = {
      id: "provider-b",
      name: "Provider B",
      settingsConfig: {},
      meta: {
        codexAgentRoleRouting: {
          enabled: true,
          frontend: {},
          backend: {},
        },
      },
    };
    const handleSubmit = vi.fn().mockResolvedValue(undefined);
    apiMocks.getCurrent.mockResolvedValue("provider-a");

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={vi.fn()}
        onSubmit={handleSubmit}
        appId="codex"
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.save" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
    expect(toastMocks.success).not.toHaveBeenCalledWith(
      "providerAdvanced.agentRoleProxyAutoEnabled",
    );
    expect(toastMocks.info).toHaveBeenCalledWith(
      "providerAdvanced.agentRoleSavedSwitchRequired",
    );
    expect(toastMocks.info).not.toHaveBeenCalledWith(
      "providerAdvanced.agentRoleRestartRequired",
    );
  });

  it("更新已关闭角色路由的 Codex Provider 后不显示角色路由提示", async () => {
    const provider: Provider = {
      id: "provider-a",
      name: "Provider A",
      settingsConfig: {},
      meta: {
        codexAgentRoleRouting: {
          enabled: false,
          frontend: {},
          backend: {},
        },
      },
    };
    const handleSubmit = vi.fn().mockResolvedValue(undefined);
    apiMocks.getCurrent.mockResolvedValue(undefined);

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={vi.fn()}
        onSubmit={handleSubmit}
        appId="codex"
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.save" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
    expect(toastMocks.success).not.toHaveBeenCalled();
    expect(toastMocks.info).not.toHaveBeenCalled();
  });

  it("更新启用角色路由的 Codex Provider 失败时保留表单且不提示", async () => {
    const provider: Provider = {
      id: "provider-a",
      name: "Provider A",
      settingsConfig: {},
      meta: {
        codexAgentRoleRouting: {
          enabled: true,
          frontend: {},
          backend: {},
        },
      },
    };
    const handleSubmit = vi.fn().mockRejectedValue(new Error("save failed"));
    const handleOpenChange = vi.fn();
    apiMocks.getCurrent.mockResolvedValue(undefined);

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={handleOpenChange}
        onSubmit={handleSubmit}
        appId="codex"
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.save" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
    expect(toastMocks.success).not.toHaveBeenCalled();
    expect(toastMocks.info).not.toHaveBeenCalled();
    expect(handleOpenChange).not.toHaveBeenCalled();
    expect(document.getElementById("provider-form")).toBeInTheDocument();
  });

  it("嵌套新增 Provider 打开时 Escape 只关闭顶层并保留编辑输入", () => {
    const provider: Provider = {
      id: "provider-a",
      name: "Provider A",
      settingsConfig: {},
    };
    const handleOpenChange = vi.fn();

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={handleOpenChange}
        onSubmit={vi.fn()}
        onAddProvider={vi.fn()}
        appId="codex"
        isProxyTakeover
      />,
    );

    const ownerDraft = screen.getByLabelText("provider-form-draft");
    fireEvent.change(ownerDraft, { target: { value: "Provider A draft" } });
    fireEvent.click(
      screen.getByRole("button", { name: "request role provider" }),
    );

    expect(
      document.getElementById("agent-role-provider-form"),
    ).toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape" });

    expect(handleOpenChange).not.toHaveBeenCalled();
    expect(document.getElementById("provider-form")).toBeInTheDocument();
    expect(ownerDraft).toHaveValue("Provider A draft");

    fireEvent.keyDown(window, { key: "Escape" });
    expect(handleOpenChange).toHaveBeenCalledWith(false);
  });
});
