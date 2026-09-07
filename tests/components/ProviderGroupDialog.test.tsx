import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ProviderGroup } from "@/types";
import { ProviderGroupDialog } from "@/components/providers/ProviderGroupDialog";

const group: ProviderGroup = {
  id: "group-1",
  appType: "codex",
  name: "Old name",
  kind: "manual",
  normalizedBaseUrl: null,
  sortIndex: 0,
  collapsed: false,
  keyPoolEnabled: false,
  keyPoolStrategy: "failover",
  keyPoolMaxRetries: 0,
  keyPoolCooldownMs: 0,
  balanceTemplateId: null,
  createdAt: 0,
  updatedAt: 0,
};

describe("ProviderGroupDialog", () => {
  it("saves a custom folder icon and color", () => {
    const onSubmit = vi.fn();
    render(
      <ProviderGroupDialog
        open
        appId="codex"
        group={group}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
      />,
    );
    fireEvent.change(screen.getByLabelText("Folder icon"), {
      target: { value: "star" },
    });
    fireEvent.change(screen.getByLabelText("Folder color"), {
      target: { value: "#22c55e" },
    });
    fireEvent.click(screen.getByRole("button", { name: /save/i }));
    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        icon: "star",
        iconColor: "#22c55e",
      }),
    );
  });

  it("submits the app and group identity with a renamed value", () => {
    const onSubmit = vi.fn();
    render(
      <ProviderGroupDialog
        open
        appId="codex"
        group={group}
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
      />,
    );

    const input = screen.getByLabelText(/folder name/i);
    fireEvent.change(input, { target: { value: "New name" } });
    fireEvent.click(screen.getByRole("button", { name: /save/i }));

    expect(onSubmit).toHaveBeenCalledWith({
      appType: "codex",
      groupId: "group-1",
      name: "New name",
      icon: null,
      iconColor: null,
    });
  });
});
