import {
  cleanup,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderBalanceSettingsDialog } from "@/components/providers/ProviderBalanceSettingsDialog";
import { providerGroupsApi } from "@/lib/api/providerGroups";
import type { BalanceQueryTemplate, Provider, ProviderGroup } from "@/types";
import { initializeSafetyI18n, renderSafetyUi } from "../utils/safetyTestUtils";

const template: BalanceQueryTemplate = {
  id: "saved-template",
  name: "Saved balance",
  method: "GET",
  path: "/balance",
  query: {},
  headers: { Authorization: "Bearer {{apiKey}}" },
  remainingPath: "/balance",
  unit: "USD",
  currency: "USD",
  balanceScope: "unknown",
  timeoutSecs: 10,
  createdAt: 1,
  updatedAt: 1,
};
const provider: Provider = { id: "key-a", name: "Key A", settingsConfig: {} };
const group: ProviderGroup = {
  id: "folder",
  appType: "codex",
  name: "My folder",
  kind: "manual",
  normalizedBaseUrl: null,
  sortIndex: 0,
  collapsed: false,
  keyPoolEnabled: false,
  keyPoolStrategy: "failover",
  keyPoolMaxRetries: 0,
  keyPoolCooldownMs: 1000,
  balanceTemplateId: template.id,
  createdAt: 1,
  updatedAt: 1,
};

function setup(member = provider, folder?: ProviderGroup) {
  const onOpenChange = vi.fn();
  const onChanged = vi.fn();
  renderSafetyUi(
    <ProviderBalanceSettingsDialog
      appId="codex"
      provider={member}
      group={folder}
      onOpenChange={onOpenChange}
      onChanged={onChanged}
    />,
  );
  return { onOpenChange, onChanged };
}

beforeEach(async () => {
  await initializeSafetyI18n();
  vi.spyOn(providerGroupsApi, "listBalanceTemplates").mockResolvedValue([
    template,
  ]);
  vi.spyOn(providerGroupsApi, "saveBalanceTemplate").mockResolvedValue(
    undefined,
  );
  vi.spyOn(providerGroupsApi, "setProviderBalanceTemplate").mockResolvedValue(
    undefined,
  );
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("provider balance template settings", () => {
  it("binds an existing template without a folder or client configuration update", async () => {
    const { onOpenChange, onChanged } = setup();
    await screen.findByRole("option", { name: template.name });
    fireEvent.change(screen.getByLabelText("Balance template"), {
      target: { value: template.id },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(providerGroupsApi.setProviderBalanceTemplate).toHaveBeenCalledWith(
      "codex",
      provider.id,
      template.id,
    );
    expect(providerGroupsApi.saveBalanceTemplate).not.toHaveBeenCalled();
    expect(onChanged).toHaveBeenCalledOnce();
  });

  it("can remove an override and inherit the folder template", async () => {
    setup(
      {
        ...provider,
        meta: { providerGroupId: group.id, balanceTemplateId: template.id },
      },
      group,
    );
    await screen.findByRole("option", { name: template.name });
    expect(
      screen.getByRole("option", { name: "Inherit from folder: My folder" }),
    ).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Balance template"), {
      target: { value: "" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(providerGroupsApi.setProviderBalanceTemplate).toHaveBeenCalledWith(
        "codex",
        provider.id,
        null,
      ),
    );
  });

  it("creates and binds a new template from an ungrouped provider", async () => {
    const { onOpenChange } = setup();
    await screen.findByRole("option", { name: template.name });
    fireEvent.click(
      screen.getByRole("button", { name: "New balance template" }),
    );
    const editor = screen
      .getAllByRole("dialog")
      .find((dialog) => within(dialog).queryByLabelText("Request path"));
    expect(editor).toBeTruthy();
    fireEvent.click(
      within(editor!).getByRole("button", { name: "Save template" }),
    );
    await waitFor(() =>
      expect(providerGroupsApi.saveBalanceTemplate).toHaveBeenCalledOnce(),
    );
    const saved = vi.mocked(providerGroupsApi.saveBalanceTemplate).mock
      .calls[0][0];
    await waitFor(() =>
      expect(providerGroupsApi.setProviderBalanceTemplate).toHaveBeenCalledWith(
        "codex",
        provider.id,
        saved.id,
      ),
    );
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("keeps the dialog open when saving the binding fails", async () => {
    vi.mocked(providerGroupsApi.setProviderBalanceTemplate).mockRejectedValue(
      new Error("Save failed"),
    );
    const { onOpenChange, onChanged } = setup();
    await screen.findByRole("option", { name: template.name });
    fireEvent.change(screen.getByLabelText("Balance template"), {
      target: { value: template.id },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(
        providerGroupsApi.setProviderBalanceTemplate,
      ).toHaveBeenCalledOnce(),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Save" })).toBeEnabled(),
    );
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(onChanged).not.toHaveBeenCalled();
  });
});
