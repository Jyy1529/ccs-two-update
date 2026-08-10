import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ProviderActions } from "@/components/providers/ProviderActions";

describe("ProviderActions transfer action", () => {
  it("exposes the transfer action for read-only source providers", () => {
    const onTransfer = vi.fn();

    render(
      <ProviderActions
        appId="hermes"
        isCurrent={false}
        isReadOnly
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDuplicate={vi.fn()}
        onTransfer={onTransfer}
        onDelete={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByTitle("导入到其他 Agent"));

    expect(onTransfer).toHaveBeenCalledTimes(1);
  });
});
