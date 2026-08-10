import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AddProviderDialog } from "@/components/providers/AddProviderDialog";
import type { ProviderFormValues } from "@/components/providers/forms/ProviderForm";

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

vi.mock("@/components/providers/forms/ProviderForm", () => ({
  ProviderForm: ({
    onSubmit,
    formId,
    onRequestAddProvider,
  }: {
    onSubmit: (values: ProviderFormValues) => void;
    formId?: string;
    onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
  }) => (
    <form
      id={formId ?? "provider-form"}
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit(mockFormValues);
      }}
    >
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

describe("AddProviderDialog", () => {
  beforeEach(() => {
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

  it("把角色路由的新增 Provider 请求传给内部表单", () => {
    const onRequestAddProvider = vi.fn();

    render(
      <AddProviderDialog
        open
        onOpenChange={vi.fn()}
        appId="codex"
        onSubmit={vi.fn()}
        onRequestAddProvider={onRequestAddProvider}
      />,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "request-role-provider" }),
    );
    expect(onRequestAddProvider).toHaveBeenCalledTimes(1);
  });

  it("角色 Provider 新增模式只显示 Codex 专属表单", () => {
    render(
      <AddProviderDialog
        open
        onOpenChange={vi.fn()}
        appId="codex"
        onSubmit={vi.fn()}
        appSpecificOnly
      />,
    );

    expect(
      screen.queryByRole("tab", { name: "provider.tabUniversal" }),
    ).not.toBeInTheDocument();
    expect(document.querySelectorAll("form")).toHaveLength(1);
  });

  it("uses a unique form id for each open dialog", async () => {
    const firstSubmit = vi.fn().mockResolvedValue(undefined);
    const secondSubmit = vi.fn().mockResolvedValue(undefined);

    render(
      <>
        <AddProviderDialog
          open
          onOpenChange={vi.fn()}
          appId="codex"
          onSubmit={firstSubmit}
        />
        <AddProviderDialog
          open
          onOpenChange={vi.fn()}
          appId="codex"
          onSubmit={secondSubmit}
        />
      </>,
    );

    const forms = Array.from(document.querySelectorAll("form"));
    expect(forms).toHaveLength(2);
    expect(forms[0]?.id).not.toBe(forms[1]?.id);

    fireEvent.click(screen.getAllByRole("button", { name: "common.add" })[1]);
    await waitFor(() => expect(secondSubmit).toHaveBeenCalledTimes(1));
    expect(firstSubmit).not.toHaveBeenCalled();
  });
});
