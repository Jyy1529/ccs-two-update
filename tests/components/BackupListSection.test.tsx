import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "@testing-library/jest-dom";

import { BackupListSection } from "@/components/settings/BackupListSection";

const mocks = vi.hoisted(() => ({
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
  toastWarning: vi.fn(),
  retryPostImportSync: vi.fn(),
  backupManager: {
    backups: [
      {
        filename: "db_backup_20260809_120000.db",
        sizeBytes: 1024,
        createdAt: "2026-08-09T12:00:00Z",
      },
    ],
    isLoading: false,
    create: vi.fn(),
    isCreating: false,
    restore: vi.fn(),
    isRestoring: false,
    rename: vi.fn(),
    isRenaming: false,
    remove: vi.fn(),
    isDeleting: false,
  },
}));

vi.mock("sonner", () => ({
  toast: {
    success: (...args: unknown[]) => mocks.toastSuccess(...args),
    error: (...args: unknown[]) => mocks.toastError(...args),
    warning: (...args: unknown[]) => mocks.toastWarning(...args),
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
  }),
}));

vi.mock("@/hooks/useBackupManager", () => ({
  useBackupManager: () => mocks.backupManager,
}));

vi.mock("@/lib/api", () => ({
  settingsApi: {
    retryPostImportSync: (...args: unknown[]) =>
      mocks.retryPostImportSync(...args),
  },
}));

vi.mock("@/components/ui/button", () => ({
  Button: ({ children, ...props }: any) => (
    <button {...props}>{children}</button>
  ),
}));

vi.mock("@/components/ui/input", () => ({
  Input: (props: any) => <input {...props} />,
}));

vi.mock("@/components/ui/label", () => ({
  Label: ({ children, ...props }: any) => <label {...props}>{children}</label>,
}));

vi.mock("@/components/ui/select", () => ({
  Select: ({ value, onValueChange, children }: any) => (
    <select
      value={value}
      onChange={(event) => onValueChange(event.target.value)}
    >
      {children}
    </select>
  ),
  SelectTrigger: ({ children }: any) => <>{children}</>,
  SelectValue: () => null,
  SelectContent: ({ children }: any) => <>{children}</>,
  SelectItem: ({ value, children }: any) => (
    <option value={value}>{children}</option>
  ),
}));

vi.mock("@/components/ui/dialog", () => ({
  Dialog: ({ open, children }: any) => (open ? <div>{children}</div> : null),
  DialogContent: ({ children }: any) => <div>{children}</div>,
  DialogDescription: ({ children }: any) => <div>{children}</div>,
  DialogFooter: ({ children }: any) => <div>{children}</div>,
  DialogHeader: ({ children }: any) => <div>{children}</div>,
  DialogTitle: ({ children }: any) => <h2>{children}</h2>,
}));

function renderSection() {
  return render(
    <BackupListSection
      backupIntervalHours={24}
      backupRetainCount={10}
      onSettingsChange={vi.fn()}
    />,
  );
}

function confirmRestore() {
  fireEvent.click(
    screen.getByRole("button", { name: "settings.backupManager.restore" }),
  );
  const restoreButtons = screen.getAllByRole("button", {
    name: "settings.backupManager.restore",
  });
  fireEvent.click(restoreButtons[restoreButtons.length - 1]);
}

describe("BackupListSection", () => {
  beforeEach(() => {
    mocks.toastSuccess.mockReset();
    mocks.toastError.mockReset();
    mocks.toastWarning.mockReset();
    mocks.retryPostImportSync.mockReset();
    mocks.backupManager.create.mockReset();
    mocks.backupManager.restore.mockReset();
    mocks.backupManager.rename.mockReset();
    mocks.backupManager.remove.mockReset();
    mocks.retryPostImportSync.mockResolvedValue(undefined);
  });

  it("shows partial success without a success toast and retries post-import sync", async () => {
    mocks.backupManager.restore.mockResolvedValueOnce({
      safetyBackupId: "safety-backup-1",
      warning: "post-import sync failed",
    });
    renderSection();

    confirmRestore();

    await waitFor(() => {
      expect(mocks.backupManager.restore).toHaveBeenCalledWith(
        "db_backup_20260809_120000.db",
      );
      expect(mocks.toastWarning).toHaveBeenCalledTimes(1);
    });
    expect(mocks.toastSuccess).not.toHaveBeenCalled();
    const [title, options] = mocks.toastWarning.mock.calls[0] as [
      string,
      {
        description: string;
        action: { label: string; onClick: () => void };
      },
    ];
    expect(title).toBe("settings.postImportSync.partialSuccess");
    expect(options.description).toContain("safety-backup-1");
    expect(options.description).toContain("post-import sync failed");
    expect(options.action.label).toBe("settings.postImportSync.retry");

    options.action.onClick();

    await waitFor(() => {
      expect(mocks.retryPostImportSync).toHaveBeenCalledTimes(1);
      expect(mocks.toastSuccess).toHaveBeenCalledWith(
        "settings.postImportSync.retrySuccess",
      );
    });
  });

  it("keeps the normal restore success toast when post-import sync succeeds", async () => {
    mocks.backupManager.restore.mockResolvedValueOnce({
      safetyBackupId: "safety-backup-2",
    });
    renderSection();

    confirmRestore();

    await waitFor(() => {
      expect(mocks.toastSuccess).toHaveBeenCalledWith(
        "settings.backupManager.restoreSuccess",
        expect.objectContaining({
          description: "settings.backupManager.safetyBackupId: safety-backup-2",
        }),
      );
    });
    expect(mocks.toastWarning).not.toHaveBeenCalled();
  });
});
