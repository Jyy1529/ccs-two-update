import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AddProviderDialog } from "@/components/providers/AddProviderDialog";
import type { ProviderFormValues } from "@/components/providers/forms/ProviderForm";

const toastMocks = vi.hoisted(() => ({
  success: vi.fn(),
  info: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
}));

vi.mock("sonner", () => ({ toast: toastMocks }));

const apiMocks = vi.hoisted(() => ({
  getCurrent: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  providersApi: {
    getCurrent: apiMocks.getCurrent,
  },
  universalProvidersApi: {
    upsert: vi.fn(),
    sync: vi.fn(),
  },
}));

vi.mock("@/components/ui/dialog", () => ({
  Dialog: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogContent: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogHeader: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
  DialogTitle: ({ children }: { children: React.ReactNode }) => (
    <h1>{children}</h1>
  ),
  DialogDescription: ({ children }: { children: React.ReactNode }) => (
    <p>{children}</p>
  ),
  DialogFooter: ({ children }: { children: React.ReactNode }) => (
    <div>{children}</div>
  ),
}));

let mockFormValues: ProviderFormValues;
const roleProviderCreatedMock = vi.hoisted(() => vi.fn());

vi.mock("@/components/providers/forms/ProviderForm", () => ({
  ProviderForm: ({
    onSubmit,
    formId = "provider-form",
    onRequestAddCodexProvider,
  }: {
    onSubmit: (values: ProviderFormValues) => Promise<void> | void;
    formId?: string;
    onRequestAddCodexProvider?: (
      onCreated: (providerId: string) => void,
    ) => void;
  }) => (
    <div>
      <form
        id={formId}
        onSubmit={(event) => {
          event.preventDefault();
          void Promise.resolve(onSubmit(mockFormValues)).catch(() => undefined);
        }}
      >
        <input aria-label={`${formId}-draft`} defaultValue="" />
      </form>
      {onRequestAddCodexProvider && (
        <button
          type="button"
          onClick={() => onRequestAddCodexProvider(roleProviderCreatedMock)}
        >
          request role provider
        </button>
      )}
    </div>
  ),
}));

describe("AddProviderDialog", () => {
  beforeEach(() => {
    roleProviderCreatedMock.mockReset();
    apiMocks.getCurrent.mockReset();
    apiMocks.getCurrent.mockResolvedValue("");
    Object.values(toastMocks).forEach((mock) => mock.mockReset());
    mockFormValues = {
      name: "Test Provider",
      websiteUrl: "https://provider.example.com",
      settingsConfig: JSON.stringify({ env: {}, config: {} }),
      meta: {
        custom_endpoints: {
          "https://api.new-endpoint.com": {
            url: "https://api.new-endpoint.com",
            addedAt: 1,
          },
        },
      },
    };
  });

  it("使用 ProviderForm 返回的自定义端点", async () => {
    const handleSubmit = vi.fn().mockResolvedValue(undefined);
    const handleOpenChange = vi.fn();

    render(
      <AddProviderDialog
        open
        onOpenChange={handleOpenChange}
        appId="claude"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "common.add",
      }),
    );

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));

    const submitted = handleSubmit.mock.calls[0][0];
    expect(submitted.meta?.custom_endpoints).toEqual(
      mockFormValues.meta?.custom_endpoints,
    );
    expect(handleOpenChange).toHaveBeenCalledWith(false);
  });

  it("在缺少自定义端点时回退到配置中的 baseUrl", async () => {
    const handleSubmit = vi.fn().mockResolvedValue(undefined);

    mockFormValues = {
      name: "Base URL Provider",
      websiteUrl: "",
      settingsConfig: JSON.stringify({
        env: { ANTHROPIC_BASE_URL: "https://claude.base" },
        config: {},
      }),
    };

    render(
      <AddProviderDialog
        open
        onOpenChange={vi.fn()}
        appId="claude"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "common.add",
      }),
    );

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));

    const submitted = handleSubmit.mock.calls[0][0];
    expect(submitted.meta?.custom_endpoints).toEqual({
      "https://claude.base": {
        url: "https://claude.base",
        addedAt: expect.any(Number),
        lastUsed: undefined,
      },
    });
  });

  it("新建 Grok Build 自定义供应商时不补默认 Grok 图标", async () => {
    const handleSubmit = vi.fn().mockResolvedValue(undefined);

    mockFormValues = {
      name: "tes 1",
      websiteUrl: "",
      icon: "",
      iconColor: "",
      settingsConfig: JSON.stringify({
        config: `[models]
default = "grok-4.5"

[model."grok-4.5"]
model = "grok-4.5"
base_url = "https://grok.example.com/v1"
name = "tes 1"
api_key = "secret"
api_backend = "responses"
context_window = 500000
`,
      }),
    };

    render(
      <AddProviderDialog
        open
        onOpenChange={vi.fn()}
        appId="grokbuild"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.add" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));

    const submitted = handleSubmit.mock.calls[0][0];
    expect(submitted.icon).toBeUndefined();
    expect(submitted.iconColor).toBeUndefined();
  });

  it("成功新建当前启用角色路由的 Codex Provider 后显示代理与重启提示", async () => {
    const createdProvider = {
      id: "provider-a",
      name: "Provider A",
      settingsConfig: {},
    };
    const handleSubmit = vi.fn().mockResolvedValue(createdProvider);
    const handleOpenChange = vi.fn();
    apiMocks.getCurrent.mockResolvedValue(createdProvider.id);
    mockFormValues.meta = {
      codexAgentRoleRouting: {
        enabled: true,
        frontend: {},
        backend: {},
      },
    };

    render(
      <AddProviderDialog
        open
        onOpenChange={handleOpenChange}
        appId="codex"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.add" }));

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

  it("成功新建非当前角色路由 Provider 后提示切换再生效", async () => {
    const createdProvider = {
      id: "provider-b",
      name: "Provider B",
      settingsConfig: {},
    };
    const handleSubmit = vi.fn().mockResolvedValue(createdProvider);
    apiMocks.getCurrent.mockResolvedValue("provider-a");
    mockFormValues.meta = {
      codexAgentRoleRouting: {
        enabled: true,
        frontend: {},
        backend: {},
      },
    };

    render(
      <AddProviderDialog
        open
        onOpenChange={vi.fn()}
        appId="codex"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.add" }));

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

  it.each([
    ["codex", false],
    ["claude", true],
  ] as const)(
    "appId=%s 且 enabled=%s 时不显示角色路由提示",
    async (appId, enabled) => {
      const handleSubmit = vi.fn().mockResolvedValue(undefined);
      mockFormValues.meta = {
        codexAgentRoleRouting: {
          enabled,
          frontend: {},
          backend: {},
        },
      };

      render(
        <AddProviderDialog
          open
          onOpenChange={vi.fn()}
          appId={appId}
          onSubmit={handleSubmit}
        />,
      );

      fireEvent.click(screen.getByRole("button", { name: "common.add" }));

      await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
      expect(toastMocks.success).not.toHaveBeenCalled();
      expect(toastMocks.info).not.toHaveBeenCalled();
    },
  );

  it("新建启用角色路由的 Codex Provider 失败时保留表单且不提示", async () => {
    const handleSubmit = vi.fn().mockRejectedValue(new Error("save failed"));
    const handleOpenChange = vi.fn();
    mockFormValues.meta = {
      codexAgentRoleRouting: {
        enabled: true,
        frontend: {},
        backend: {},
      },
    };

    render(
      <AddProviderDialog
        open
        onOpenChange={handleOpenChange}
        appId="codex"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "common.add" }));

    await waitFor(() => expect(handleSubmit).toHaveBeenCalledTimes(1));
    expect(toastMocks.success).not.toHaveBeenCalled();
    expect(toastMocks.info).not.toHaveBeenCalled();
    expect(handleOpenChange).not.toHaveBeenCalled();
    expect(document.getElementById("provider-form")).toBeInTheDocument();
  });

  it("新增前端 Provider 时保持拥有者表单挂载并自动选择新 Provider", async () => {
    const createdProvider = {
      id: "provider-b",
      name: "Provider B",
      settingsConfig: {},
    };
    const handleSubmit = vi.fn().mockResolvedValue(createdProvider);

    render(
      <AddProviderDialog
        open
        onOpenChange={vi.fn()}
        appId="codex"
        onSubmit={handleSubmit}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "request role provider" }),
    );

    expect(document.getElementById("provider-form")).toBeInTheDocument();
    expect(
      document.getElementById("agent-role-provider-form"),
    ).toBeInTheDocument();

    const addButtons = screen.getAllByRole("button", { name: "common.add" });
    fireEvent.click(addButtons[addButtons.length - 1]);

    await waitFor(() => {
      expect(roleProviderCreatedMock).toHaveBeenCalledWith("provider-b");
    });
    expect(document.getElementById("provider-form")).toBeInTheDocument();
  });

  it("嵌套新增 Provider 打开时 Escape 只关闭顶层并保留拥有者输入", async () => {
    const handleOpenChange = vi.fn();

    render(
      <AddProviderDialog
        open
        onOpenChange={handleOpenChange}
        appId="codex"
        onSubmit={vi.fn()}
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
