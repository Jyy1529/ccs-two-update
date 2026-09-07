import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { AppManagementSettings } from "@/components/settings/AppManagementSettings";
import { AppVisibilitySettings } from "@/components/settings/AppVisibilitySettings";
import { APP_IDS, DEFAULT_VISIBLE_APPS } from "@/config/appConfig";
import {
  appManagementApi,
  type AppManagementPreview,
} from "@/lib/api/appManagement";
import type { SettingsFormState } from "@/hooks/useSettings";
import {
  initializeSafetyI18n,
  managementFixture,
  renderSafetyUi,
} from "../utils/safetyTestUtils";

const previewFixture = (enabled = false): AppManagementPreview => ({
  id: "preview-1",
  appId: "codex",
  enabled,
  revision: "management-1",
  files: [{ path: "D:/isolated/config.toml", action: "release owned fields" }],
  warnings: [],
  conflicts: [],
});

beforeEach(async () => {
  await initializeSafetyI18n();
});
afterEach(() => vi.restoreAllMocks());

describe("independent app management controls", () => {
  it("renders all ten switches independently of app visibility and only applies after preview confirmation", async () => {
    const original = managementFixture();
    const next = managementFixture({
      codex: { enabled: false, phase: "unmanaged" },
    });
    const visibilityChange = vi.fn();
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(original);
    const prepare = vi
      .spyOn(appManagementApi, "preview")
      .mockResolvedValue(previewFixture());
    const apply = vi.spyOn(appManagementApi, "apply").mockResolvedValue(next);
    renderSafetyUi(
      <>
        <AppVisibilitySettings
          settings={
            {
              visibleApps: { ...DEFAULT_VISIBLE_APPS, codex: false },
            } as SettingsFormState
          }
          onChange={visibilityChange}
        />
        <AppManagementSettings />
      </>,
      original,
    );
    const section = screen.getByRole("region", { name: "App management" });
    expect(within(section).getAllByRole("switch")).toHaveLength(10);
    expect(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    ).toBeChecked();
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    await screen.findByRole("dialog", { name: "Codex management change" });
    expect(prepare).toHaveBeenCalledWith("codex", false);
    expect(apply).not.toHaveBeenCalled();
    fireEvent.click(
      screen.getByRole("button", {
        name: "Stop management and safely release",
      }),
    );
    await waitFor(() =>
      expect(
        screen.getByRole("switch", { name: "Manage Codex with ccs" }),
      ).not.toBeChecked(),
    );
    expect(apply).toHaveBeenCalledWith("preview-1");
    expect(visibilityChange).not.toHaveBeenCalled();
    expect(
      screen.getByRole("switch", { name: "Manage Pi with ccs" }),
    ).toBeChecked();
  });

  it("allows the last managed app to be disabled", async () => {
    const original = managementFixture();
    original.apps.forEach((app) => {
      if (app.appId !== "codex") {
        app.enabled = false;
        app.phase = "unmanaged";
      }
    });
    const next = managementFixture();
    next.apps.forEach((app) => {
      app.enabled = false;
      app.phase = "unmanaged";
    });
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(original);
    vi.spyOn(appManagementApi, "preview").mockResolvedValue(previewFixture());
    vi.spyOn(appManagementApi, "apply").mockResolvedValue(next);
    renderSafetyUi(<AppManagementSettings />, original);
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Stop management and safely release",
      }),
    );
    await waitFor(() =>
      expect(
        screen
          .getAllByRole("switch")
          .every((toggle) => toggle.getAttribute("aria-checked") === "false"),
      ).toBe(true),
    );
  });

  it("shows incomplete release instead of claiming success, and leaves other apps enabled", async () => {
    const original = managementFixture();
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(original);
    vi.spyOn(appManagementApi, "preview").mockResolvedValue({
      ...previewFixture(),
      conflicts: ["Shared directory needs isolation"],
    });
    vi.spyOn(appManagementApi, "apply").mockResolvedValue(
      managementFixture({
        codex: {
          enabled: false,
          phase: "pending_release",
          message: "External changes preserved",
        },
      }),
    );
    renderSafetyUi(<AppManagementSettings />, original);
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    expect(
      await screen.findByText("Shared directory needs isolation"),
    ).toBeVisible();
    const stopButton = screen.getByRole("button", {
      name: "Stop management; release pending",
    });
    expect(stopButton).toBeEnabled();
    fireEvent.click(stopButton);
    expect(
      await screen.findByText("Writes stopped; release pending"),
    ).toBeVisible();
    expect(screen.getByRole("button", { name: "Retry release" })).toBeEnabled();
    expect(
      screen.getByRole("switch", { name: "Manage OpenCode with ccs" }),
    ).toBeChecked();
  });

  it("rejects a stale management preview", async () => {
    const original = managementFixture();
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(original);
    vi.spyOn(appManagementApi, "preview").mockResolvedValue(previewFixture());
    const apply = vi.spyOn(appManagementApi, "apply");
    const { queryClient } = renderSafetyUi(<AppManagementSettings />, original);
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    await screen.findByRole("dialog");
    act(() =>
      queryClient.setQueryData(["appManagement"], {
        ...original,
        revision: "management-2",
      }),
    );
    expect(
      await screen.findByText(
        "Management state changed. Preview the change again.",
      ),
    ).toBeVisible();
    expect(
      screen.queryByRole("button", {
        name: "Stop management and safely release",
      }),
    ).not.toBeInTheDocument();
    expect(apply).not.toHaveBeenCalled();
  });

  it("does not let an older in-flight read restore management after disabling", async () => {
    const original = managementFixture();
    const stopped = managementFixture({
      codex: { enabled: false, phase: "unmanaged" },
    });
    let finishRead!: (state: typeof original) => void;
    vi.spyOn(appManagementApi, "getState").mockImplementation(
      () =>
        new Promise((resolve) => {
          finishRead = resolve;
        }),
    );
    vi.spyOn(appManagementApi, "preview").mockResolvedValue(previewFixture());
    vi.spyOn(appManagementApi, "apply").mockResolvedValue(stopped);
    const { queryClient } = renderSafetyUi(<AppManagementSettings />, original);
    act(() => {
      void queryClient.invalidateQueries({ queryKey: ["appManagement"] });
    });
    await waitFor(() => expect(appManagementApi.getState).toHaveBeenCalled());
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Stop management and safely release",
      }),
    );
    await waitFor(() =>
      expect(
        screen.getByRole("switch", { name: "Manage Codex with ccs" }),
      ).not.toBeChecked(),
    );
    await act(async () => finishRead(original));
    expect(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    ).not.toBeChecked();
    expect(queryClient.getQueryData(["appManagement"])).toEqual(stopped);
  });

  it("reads the persisted stopped state after a release operation reports failure", async () => {
    const original = managementFixture();
    const stopped = managementFixture({
      codex: { enabled: false, phase: "pending_release" },
    });
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(stopped);
    vi.spyOn(appManagementApi, "preview").mockResolvedValue(previewFixture());
    vi.spyOn(appManagementApi, "apply").mockRejectedValue(
      new Error("Detachment requires review"),
    );
    renderSafetyUi(<AppManagementSettings />, original);
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Stop management and safely release",
      }),
    );
    expect(await screen.findByText("Detachment requires review")).toBeVisible();
    expect(
      await screen.findByText("Writes stopped; release pending"),
    ).toBeVisible();
    expect(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    ).not.toBeChecked();
  });

  it("preserves pending-review state after reenabling and does not invoke file application", async () => {
    const original = managementFixture({
      codex: { enabled: false, phase: "unmanaged" },
    });
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(original);
    vi.spyOn(appManagementApi, "preview").mockResolvedValue(
      previewFixture(true),
    );
    vi.spyOn(appManagementApi, "apply").mockResolvedValue(
      managementFixture({ codex: { phase: "pending_review" } }),
    );
    renderSafetyUi(<AppManagementSettings />, original);
    fireEvent.click(
      screen.getByRole("switch", { name: "Manage Codex with ccs" }),
    );
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Enable and review differences",
      }),
    );
    expect(
      await screen.findByText("Enabled; configuration review pending"),
    ).toBeVisible();
  });

  it("fails closed when the authority cannot be read", async () => {
    vi.spyOn(appManagementApi, "getState").mockRejectedValue(
      new Error("unreadable"),
    );
    const prepare = vi.spyOn(appManagementApi, "preview");
    renderSafetyUi(<AppManagementSettings />);
    await screen.findByRole("alert");
    expect(screen.getAllByRole("switch")).toHaveLength(APP_IDS.length);
    screen
      .getAllByRole("switch")
      .forEach((toggle) => expect(toggle).toBeDisabled());
    expect(prepare).not.toHaveBeenCalled();
  });
});
