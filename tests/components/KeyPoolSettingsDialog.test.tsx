import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ProviderGroupStatus } from "@/types";
import { KeyPoolSettingsDialog } from "@/components/providers/KeyPoolSettingsDialog";

const status: ProviderGroupStatus = {
  group: {
    id: "group-1",
    appType: "codex",
    name: "AgentRouter",
    kind: "manual",
    normalizedBaseUrl: null,
    sortIndex: 0,
    collapsed: false,
    keyPoolEnabled: true,
    keyPoolStrategy: "failover",
    keyPoolMaxRetries: 1,
    keyPoolCooldownMs: 1000,
    balanceTemplateId: null,
    createdAt: 1,
    updatedAt: 1,
  },
  members: [
    {
      providerId: "p1",
      providerName: "Key one",
      sortIndex: 0,
      keyPoolEnabled: true,
      eligible: true,
      coolingDown: false,
      error: null,
    },
    {
      providerId: "p2",
      providerName: "Key two",
      sortIndex: 1,
      keyPoolEnabled: false,
      eligible: false,
      coolingDown: false,
      error: null,
    },
  ],
  eligibleMemberCount: 1,
};

describe("KeyPoolSettingsDialog", () => {
  it("submits strategy, retries, cooldown, and member changes", () => {
    const onSave = vi.fn();
    const onMemberEnabledChange = vi.fn();
    render(
      <KeyPoolSettingsDialog
        open
        status={status}
        onOpenChange={vi.fn()}
        onSave={onSave}
        onMemberEnabledChange={onMemberEnabledChange}
      />,
    );

    fireEvent.click(screen.getByRole("radio", { name: "Round robin" }));
    fireEvent.change(screen.getByLabelText("Extra retries per Key"), {
      target: { value: "2" },
    });
    fireEvent.change(screen.getByLabelText("Cooldown milliseconds"), {
      target: { value: "5000" },
    });
    fireEvent.click(screen.getByRole("switch", { name: "Key two" }));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(onMemberEnabledChange).toHaveBeenCalledWith("p2", true);
    expect(onSave).toHaveBeenCalledWith({
      enabled: true,
      strategy: "round_robin",
      maxRetries: 2,
      cooldownMs: 5000,
    });
  });

  it("refreshes cooldown state without overwriting unsaved policy edits", () => {
    const props = {
      onOpenChange: vi.fn(),
      onSave: vi.fn(),
      onMemberEnabledChange: vi.fn(),
      onMemberMove: vi.fn(),
    };
    const { rerender } = render(
      <KeyPoolSettingsDialog open status={status} {...props} />,
    );
    fireEvent.click(screen.getByRole("radio", { name: "Round robin" }));
    fireEvent.change(screen.getByLabelText("Extra retries per Key"), {
      target: { value: "2" },
    });
    rerender(
      <KeyPoolSettingsDialog
        open
        status={{
          ...status,
          members: [
            {
              ...status.members[0],
              coolingDown: true,
              cooldownRemainingMs: 3500,
              consecutiveFailures: 1,
            },
            status.members[1],
          ],
        }}
        {...props}
      />,
    );
    expect(screen.getByText("Cooling down (4s)")).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "Round robin" })).toBeChecked();
    expect(screen.getByLabelText("Extra retries per Key")).toHaveValue(2);
    fireEvent.click(screen.getByRole("button", { name: "Move Key one down" }));
    expect(props.onMemberMove).toHaveBeenCalledWith("p1", 1);
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(props.onSave).toHaveBeenCalledWith(
      expect.objectContaining({ strategy: "round_robin", maxRetries: 2 }),
    );
  });
});
