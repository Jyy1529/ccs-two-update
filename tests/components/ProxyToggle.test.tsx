import { fireEvent, screen } from "@testing-library/react";
import {
  managementFixture,
  renderManagedUi as render,
} from "../utils/safetyTestUtils";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ProxyToggle } from "@/components/proxy/ProxyToggle";

const useProxyStatusMock = vi.hoisted(() => vi.fn());

vi.mock("@/hooks/useProxyStatus", () => ({
  useProxyStatus: useProxyStatusMock,
}));

describe("ProxyToggle", () => {
  beforeEach(() => {
    useProxyStatusMock.mockReset();
  });

  it("waits for initial proxy status before allowing takeover", () => {
    const proxyState = {
      isRunning: false,
      takeoverStatus: undefined,
      setTakeoverForApp: vi.fn(),
      isPending: false,
      isInitialStatusPending: true,
      status: undefined,
    };
    useProxyStatusMock.mockImplementation(() => proxyState);
    const { rerender } = render(<ProxyToggle activeApp="claude" />);

    expect(screen.getByRole("switch")).toBeDisabled();

    proxyState.isInitialStatusPending = false;
    rerender(<ProxyToggle activeApp="claude" />);

    expect(screen.getByRole("switch")).toBeEnabled();
  });

  it("cannot take over an unmanaged app after proxy status is ready", () => {
    const setTakeoverForApp = vi.fn();
    useProxyStatusMock.mockReturnValue({
      isRunning: true,
      takeoverStatus: {},
      setTakeoverForApp,
      isPending: false,
      isInitialStatusPending: false,
      status: undefined,
    });
    render(
      <ProxyToggle activeApp="codex" />,
      managementFixture({ codex: { enabled: false, phase: "unmanaged" } }),
    );
    const toggle = screen.getByRole("switch");
    expect(toggle).toBeDisabled();
    fireEvent.click(toggle);
    expect(setTakeoverForApp).not.toHaveBeenCalled();
  });
});
