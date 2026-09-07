import { screen, waitFor } from "@testing-library/react";
import {
  managementFixture,
  renderManagedUi as render,
} from "../utils/safetyTestUtils";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CodexRepairSettings } from "@/components/settings/CodexRepairSettings";
import { settingsApi } from "@/lib/api";

const toastSuccessMock = vi.fn();
const toastErrorMock = vi.fn();

vi.mock("sonner", () => ({
  toast: {
    success: (...args: unknown[]) => toastSuccessMock(...args),
    error: (...args: unknown[]) => toastErrorMock(...args),
  },
}));

vi.mock("@/lib/api", () => ({
  settingsApi: {
    getCodexRepairStatus: vi.fn(),
    launchCodexRepair: vi.fn(),
  },
}));

const statusMock = vi.mocked(settingsApi.getCodexRepairStatus);
const launchMock = vi.mocked(settingsApi.launchCodexRepair);

describe("CodexRepairSettings", () => {
  beforeEach(() => {
    statusMock.mockReset();
    launchMock.mockReset();
    toastSuccessMock.mockReset();
    toastErrorMock.mockReset();
    statusMock.mockResolvedValue({
      state: "needsRepair",
      platformSupported: true,
      codexInstalled: true,
      runtimeInstalled: true,
      repairRunning: false,
      lastRepairError: null,
      packageVersion: "26.715.4045.0",
      warnings: ["config pinned mismatch"],
      checkedAt: "2026-07-19T10:00:00+08:00",
      runtimeCommit: "cb6712f7e17f1c4082c0ad9a39ce225fb895d922",
    });
    launchMock.mockResolvedValue({ started: true, runtimeInstalled: true });
  });

  it("checks automatically when enabled and exposes the detection toggle", async () => {
    const onEnabledChange = vi.fn();
    render(<CodexRepairSettings enabled onEnabledChange={onEnabledChange} />);

    expect(await screen.findByText("config pinned mismatch")).toBeVisible();
    expect(screen.getByText("26.715.4045.0")).toBeVisible();
    expect(statusMock).toHaveBeenCalledTimes(1);

    await userEvent.click(
      screen.getByRole("switch", { name: /Codex Desktop/i }),
    );
    expect(onEnabledChange).toHaveBeenCalledWith(false);
  });

  it("keeps read-only detection available but disables repair for unmanaged Codex", async () => {
    render(
      <CodexRepairSettings enabled onEnabledChange={vi.fn()} />,
      managementFixture({ codex: { enabled: false, phase: "unmanaged" } }),
    );
    await screen.findByText("config pinned mismatch");
    expect(
      screen.getByRole("button", {
        name: /管理员修复|Repair as administrator/i,
      }),
    ).toBeDisabled();
    expect(statusMock).toHaveBeenCalledTimes(1);
    expect(launchMock).not.toHaveBeenCalled();
  });

  it("requires confirmation before launching the administrator repair", async () => {
    render(<CodexRepairSettings enabled onEnabledChange={vi.fn()} />);
    await screen.findByText("config pinned mismatch");

    await userEvent.click(
      screen.getByRole("button", {
        name: /管理员修复|Repair as administrator/i,
      }),
    );
    expect(screen.getByRole("alertdialog")).toBeVisible();
    expect(launchMock).not.toHaveBeenCalled();

    await userEvent.click(
      screen.getByRole("button", { name: /确认修复|Start repair/i }),
    );
    await waitFor(() => expect(launchMock).toHaveBeenCalledTimes(1));
    expect(toastSuccessMock).toHaveBeenCalledWith(
      expect.stringMatching(
        /已请求管理员权限|Administrator permission requested/i,
      ),
    );
    expect(
      screen.getByRole("button", { name: /修复进行中|Repair running/i }),
    ).toBeDisabled();
    expect(
      screen.getByText(/独立管理员窗口|administrator window/i),
    ).toBeVisible();
  });

  it("shows the backend error when administrator launch fails", async () => {
    launchMock.mockRejectedValue("The operation was canceled by the user");
    render(<CodexRepairSettings enabled onEnabledChange={vi.fn()} />);
    await screen.findByText("config pinned mismatch");

    await userEvent.click(
      screen.getByRole("button", {
        name: /管理员修复|Repair as administrator/i,
      }),
    );
    await userEvent.click(
      screen.getByRole("button", { name: /确认修复|Start repair/i }),
    );

    await waitFor(() =>
      expect(toastErrorMock).toHaveBeenCalledWith(
        expect.stringMatching(/无法启动管理员修复|Could not start/i),
        expect.objectContaining({
          description: "The operation was canceled by the user",
        }),
      ),
    );
  });
});
