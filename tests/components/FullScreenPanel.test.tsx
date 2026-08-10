import { fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FullScreenPanel } from "@/components/common/FullScreenPanel";

describe("FullScreenPanel stacking", () => {
  beforeEach(() => {
    document.body.style.overflow = "auto";
  });

  afterEach(() => {
    document.body.style.overflow = "";
  });

  it("lets only the top panel handle Escape", () => {
    const closeOwner = vi.fn();
    const closeChild = vi.fn();

    render(
      <>
        <FullScreenPanel
          isOpen
          title="Owner"
          onClose={closeOwner}
          escapeEnabled={false}
        >
          owner
        </FullScreenPanel>
        <FullScreenPanel isOpen title="Child" onClose={closeChild}>
          child
        </FullScreenPanel>
      </>,
    );

    fireEvent.keyDown(window, { key: "Escape" });

    expect(closeChild).toHaveBeenCalledTimes(1);
    expect(closeOwner).not.toHaveBeenCalled();
  });

  it("restores the previous scroll lock when a stacked panel closes", () => {
    const { rerender } = render(
      <>
        <FullScreenPanel isOpen title="Owner" onClose={vi.fn()}>
          owner
        </FullScreenPanel>
        <FullScreenPanel isOpen title="Child" onClose={vi.fn()}>
          child
        </FullScreenPanel>
      </>,
    );

    expect(document.body.style.overflow).toBe("hidden");

    rerender(
      <>
        <FullScreenPanel isOpen title="Owner" onClose={vi.fn()}>
          owner
        </FullScreenPanel>
        <FullScreenPanel isOpen={false} title="Child" onClose={vi.fn()}>
          child
        </FullScreenPanel>
      </>,
    );
    expect(document.body.style.overflow).toBe("hidden");

    rerender(
      <>
        <FullScreenPanel isOpen={false} title="Owner" onClose={vi.fn()}>
          owner
        </FullScreenPanel>
        <FullScreenPanel isOpen={false} title="Child" onClose={vi.fn()}>
          child
        </FullScreenPanel>
      </>,
    );
    expect(document.body.style.overflow).toBe("auto");
  });
});
