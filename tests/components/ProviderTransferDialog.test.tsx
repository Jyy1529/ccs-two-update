import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Provider } from "@/types";
import { ProviderTransferDialog } from "@/components/providers/ProviderTransferDialog";
import { providersApi } from "@/lib/api/providers";

const toastMocks = vi.hoisted(() => ({
  success: vi.fn(),
  warning: vi.fn(),
  error: vi.fn(),
}));

vi.mock("sonner", () => ({ toast: toastMocks }));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: { defaultValue?: string }) =>
      options?.defaultValue ?? key,
  }),
}));

vi.mock("@/components/ProviderIcon", () => ({
  ProviderIcon: ({ name }: { name: string }) => <span>{name}</span>,
}));

vi.mock("@/lib/api/providers", () => ({
  providersApi: {
    getTransferPreview: vi.fn(),
    transferToApps: vi.fn(),
  },
}));

const getTransferPreviewMock = vi.mocked(providersApi.getTransferPreview);
const transferToAppsMock = vi.mocked(providersApi.transferToApps);

const sourceProvider: Provider = {
  id: "glos-source",
  name: "glos",
  settingsConfig: {},
};

function renderDialog() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });

  const view = render(
    <QueryClientProvider client={queryClient}>
      <ProviderTransferDialog
        open
        sourceApp="claude"
        sourceProvider={sourceProvider}
        onOpenChange={vi.fn()}
      />
    </QueryClientProvider>,
  );
  return { ...view, queryClient };
}

describe("ProviderTransferDialog", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    getTransferPreviewMock.mockResolvedValue({
      name: "glos",
      notes: "company account",
      websiteUrl: "https://example.com",
      baseUrl: "https://api.example.com/v1",
      hasApiKey: true,
    });
    transferToAppsMock.mockResolvedValue([
      {
        appId: "codex",
        status: "created",
        providerId: "glos-codex",
        providerName: "glos",
      },
      {
        appId: "opencode",
        status: "created",
        providerId: "glos-opencode",
        providerName: "glos",
      },
    ]);
  });

  it("excludes the source app, masks the key, and transfers selected targets", async () => {
    const { queryClient } = renderDialog();
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");

    await waitFor(() => expect(getTransferPreviewMock).toHaveBeenCalled());

    expect(screen.queryByRole("button", { name: "Claude Code" })).toBeNull();
    expect(screen.getByText("************")).toBeInTheDocument();

    const importButton = screen.getByRole("button", { name: "导入" });
    expect(importButton).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: "Codex" }));
    fireEvent.click(screen.getByRole("button", { name: "OpenCode" }));
    expect(importButton).toBeEnabled();

    fireEvent.click(importButton);

    await waitFor(() =>
      expect(transferToAppsMock).toHaveBeenCalledWith({
        sourceApp: "claude",
        sourceProviderId: "glos-source",
        targetApps: ["codex", "opencode"],
      }),
    );
    await waitFor(() => expect(invalidateSpy).toHaveBeenCalledTimes(2));
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: ["providers", "codex"],
    });
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: ["providers", "opencode"],
    });
    expect(toastMocks.success).toHaveBeenCalledTimes(1);
  });

  it("shows created and failed results for a partial transfer", async () => {
    transferToAppsMock.mockResolvedValueOnce([
      {
        appId: "codex",
        status: "created",
        providerId: "glos-codex",
        providerName: "glos (导入)",
      },
      {
        appId: "opencode",
        status: "failed",
        message: "Invalid endpoint",
      },
    ]);
    renderDialog();
    await screen.findByText("company account");

    fireEvent.click(screen.getByRole("button", { name: "Codex" }));
    fireEvent.click(screen.getByRole("button", { name: "OpenCode" }));
    fireEvent.click(screen.getByRole("button", { name: "导入" }));

    expect(await screen.findByText("glos (导入)")).toBeInTheDocument();
    expect(screen.getByText("Invalid endpoint")).toBeInTheDocument();
    expect(toastMocks.warning).toHaveBeenCalledTimes(1);
  });

  it("shows every selected target as failed when the request rejects", async () => {
    transferToAppsMock.mockRejectedValueOnce(new Error("Source is invalid"));
    renderDialog();
    await screen.findByText("company account");

    fireEvent.click(screen.getByRole("button", { name: "Codex" }));
    fireEvent.click(screen.getByRole("button", { name: "Gemini" }));
    fireEvent.click(screen.getByRole("button", { name: "导入" }));

    await waitFor(() =>
      expect(screen.getAllByText("Source is invalid")).toHaveLength(2),
    );
    expect(toastMocks.error).toHaveBeenCalledTimes(1);
  });
});
