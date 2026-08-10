import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Provider } from "@/types";

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

vi.mock("@/components/common/FullScreenPanel", () => ({
  FullScreenPanel: ({
    isOpen,
    children,
    footer,
  }: {
    isOpen: boolean;
    children: React.ReactNode;
    footer?: React.ReactNode;
  }) =>
    isOpen ? (
      <div>
        <div>{children}</div>
        <div>{footer}</div>
      </div>
    ) : null,
}));

vi.mock("@/components/providers/forms/ProviderForm", () => ({
  ProviderForm: ({
    initialData,
    onSubmit,
    isProxyTakeover,
    formId,
    onRequestAddProvider,
  }: {
    initialData: {
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
    }) => void;
    isProxyTakeover?: boolean;
    formId?: string;
    onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
  }) => (
    <form
      id={formId ?? "provider-form"}
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit({
          name: initialData.name ?? "",
          websiteUrl: initialData.websiteUrl ?? "",
          notes: initialData.notes,
          settingsConfig: JSON.stringify(initialData.settingsConfig ?? {}),
          meta: initialData.meta,
          icon: initialData.icon,
          iconColor: initialData.iconColor,
        });
      }}
    >
      <output data-testid="settings-config">
        {JSON.stringify(initialData.settingsConfig ?? {})}
      </output>
      <output data-testid="is-proxy-takeover">
        {isProxyTakeover ? "true" : "false"}
      </output>
      {onRequestAddProvider && (
        <button
          type="button"
          onClick={() => onRequestAddProvider(() => undefined)}
        >
          request-role-provider
        </button>
      )}
    </form>
  ),
}));

import { EditProviderDialog } from "@/components/providers/EditProviderDialog";

describe("EditProviderDialog", () => {
  beforeEach(() => {
    apiMocks.getCurrent.mockReset();
    apiMocks.getLiveProviderSettings.mockReset();
    apiMocks.getOpenClawLiveProvider.mockReset();
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

  it("把角色路由的新增 Provider 请求传给内部表单", async () => {
    const provider: Provider = {
      id: "owner-a",
      name: "Owner A",
      settingsConfig: {},
    };
    const onRequestAddProvider = vi.fn();
    apiMocks.getCurrent.mockResolvedValue(null);

    render(
      <EditProviderDialog
        open
        provider={provider}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        appId="codex"
        onRequestAddProvider={onRequestAddProvider}
      />,
    );

    const requestButton = await screen.findByRole("button", {
      name: "request-role-provider",
    });
    fireEvent.click(requestButton);
    expect(onRequestAddProvider).toHaveBeenCalledTimes(1);
  });

  it("uses a unique form id for each open dialog", async () => {
    apiMocks.getCurrent.mockResolvedValue(null);
    const firstSubmit = vi.fn().mockResolvedValue(undefined);
    const secondSubmit = vi.fn().mockResolvedValue(undefined);
    const firstProvider: Provider = {
      id: "provider-a",
      name: "Provider A",
      settingsConfig: {},
    };
    const secondProvider: Provider = {
      id: "provider-b",
      name: "Provider B",
      settingsConfig: {},
    };

    render(
      <>
        <EditProviderDialog
          open
          provider={firstProvider}
          onOpenChange={vi.fn()}
          onSubmit={firstSubmit}
          appId="codex"
        />
        <EditProviderDialog
          open
          provider={secondProvider}
          onOpenChange={vi.fn()}
          onSubmit={secondSubmit}
          appId="codex"
        />
      </>,
    );

    const forms = Array.from(document.querySelectorAll("form"));
    expect(forms).toHaveLength(2);
    expect(forms[0]?.id).not.toBe(forms[1]?.id);

    fireEvent.click(screen.getAllByRole("button", { name: "common.save" })[1]);
    await waitFor(() => expect(secondSubmit).toHaveBeenCalledTimes(1));
    expect(firstSubmit).not.toHaveBeenCalled();
  });
});
