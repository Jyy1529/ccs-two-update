import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ProviderGroup } from "@/types";
import {
  ProviderGroupHeader,
  type ProviderGroupHeaderProps,
} from "@/components/providers/ProviderGroupHeader";

const group: ProviderGroup = {
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
  createdAt: 0,
  updatedAt: 0,
};

describe("ProviderGroupHeader", () => {
  it("binds pointer and keyboard activators without enabling native HTML dragging", () => {
    const dragHandleProps: NonNullable<
      ProviderGroupHeaderProps["dragHandleProps"]
    > = {
      attributes: {
        role: "button",
        tabIndex: 0,
        "aria-disabled": false,
        "aria-pressed": false,
        "aria-roledescription": "sortable",
        "aria-describedby": "sort-instructions",
      },
      listeners: { onPointerDown: vi.fn(), onKeyDown: vi.fn() },
      setActivatorNodeRef: vi.fn(),
      isDragging: false,
      disabled: false,
    };
    const props: ProviderGroupHeaderProps = {
      group,
      memberCount: 2,
      onToggleCollapsed: vi.fn(),
      onRename: vi.fn(),
      onDelete: vi.fn(),
      onConfigurePool: vi.fn(),
      onQueryBalances: vi.fn(),
      dragHandleProps,
    };
    const { rerender } = render(<ProviderGroupHeader {...props} />);
    const handle = screen.getByRole("button", {
      name: "Drag folder AgentRouter",
    });
    expect(handle).not.toHaveAttribute("draggable", "true");
    expect(handle).toHaveClass("touch-none");
    expect(dragHandleProps.setActivatorNodeRef).toHaveBeenCalledWith(handle);
    fireEvent.pointerDown(handle);
    fireEvent.keyDown(handle, { code: "Space" });
    expect(dragHandleProps.listeners?.onPointerDown).toHaveBeenCalledTimes(1);
    expect(dragHandleProps.listeners?.onKeyDown).toHaveBeenCalledTimes(1);
    expect(props.onToggleCollapsed).not.toHaveBeenCalled();
    rerender(
      <ProviderGroupHeader
        {...props}
        dragHandleProps={{ ...dragHandleProps, disabled: true }}
      />,
    );
    expect(handle).toBeDisabled();
  });

  it("renders the group name, member count, and pool strategy", () => {
    render(
      <ProviderGroupHeader
        group={group}
        memberCount={2}
        onToggleCollapsed={vi.fn()}
        onRename={vi.fn()}
        onDelete={vi.fn()}
        onConfigurePool={vi.fn()}
        onQueryBalances={vi.fn()}
      />,
    );

    expect(screen.getByText("AgentRouter")).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument();
    expect(screen.getByText(/failover/i)).toBeInTheDocument();
  });

  it("routes the pool action to the caller", () => {
    const onConfigurePool = vi.fn();
    render(
      <ProviderGroupHeader
        group={group}
        memberCount={2}
        onToggleCollapsed={vi.fn()}
        onRename={vi.fn()}
        onDelete={vi.fn()}
        onConfigurePool={onConfigurePool}
        onQueryBalances={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /key pool/i }));
    expect(onConfigurePool).toHaveBeenCalledTimes(1);
  });

  it("routes the balance action to the caller", () => {
    const onQueryBalances = vi.fn();
    render(
      <ProviderGroupHeader
        group={group}
        memberCount={2}
        onToggleCollapsed={vi.fn()}
        onRename={vi.fn()}
        onDelete={vi.fn()}
        onConfigurePool={vi.fn()}
        onQueryBalances={onQueryBalances}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Query balances" }));
    expect(onQueryBalances).toHaveBeenCalledTimes(1);
  });
});
