import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { ConfigGuardPanel } from "@/components/management/ConfigGuardPanel";
import {
  appManagementApi,
  configGuardApi,
  type ConfigChangePreview,
  type ConfigGuardState,
  type GuardAudit,
  type GuardBackup,
} from "@/lib/api/appManagement";
import {
  guardFixture,
  initializeSafetyI18n,
  managementFixture,
  renderSafetyUi,
} from "../utils/safetyTestUtils";

const previewFixture = (): ConfigChangePreview => ({
  id: "backend-preview",
  appId: "codex",
  path: "D:/isolated/codex/config.toml",
  conflicts: ["model has an external edit"],
  changes: [
    {
      path: "model",
      kind: "replace",
      before: "local-model",
      after: "db-model",
    },
  ],
  revision: "file-1",
});

const backupFixture = (overrides: Partial<GuardBackup> = {}): GuardBackup => ({
  id: "backup-config",
  fileId: "backend-file-id",
  path: "D:/isolated/codex/config.toml",
  createdAt: "2026-09-06T08:00:00Z",
  source: "provider_switch",
  fields: ["model", "model_providers.ccs.base_url"],
  beforeRevision: "before-hash",
  afterRevision: "after-hash",
  groupId: "linked-connection-group",
  ...overrides,
});

const backupStateFixture = (): ConfigGuardState => ({
  ...guardFixture(),
  backups: [
    backupFixture(),
    backupFixture({
      id: "backup-auth",
      fileId: "backend-auth-file-id",
      path: "D:/isolated/codex/auth.json",
      fields: ["OPENAI_API_KEY"],
    }),
  ],
  history: [],
});

const restorePreviewFixture = (): ConfigChangePreview => ({
  ...previewFixture(),
  id: "backend-group-restore-preview",
  conflicts: [],
  changes: [
    {
      path: "OPENAI_API_KEY",
      kind: "replace",
      // The restoration UI must not render value fields, even if an unexpected
      // backend response includes them. These are synthetic test strings only.
      before: "synthetic-current-secret",
      after: "synthetic-backup-secret",
    },
  ],
});

const externalChangeFixture = (): ConfigChangePreview => ({
  ...previewFixture(),
  id: "backend-external-change-preview",
  revision: "external-file-hash",
  conflicts: ["Configuration changed outside ccs"],
  changes: [
    {
      path: "model_catalog_json",
      kind: "replace",
      before: "string (redacted)",
      after: "string (redacted)",
    },
  ],
});

beforeEach(async () => {
  await initializeSafetyI18n();
  vi.spyOn(configGuardApi, "getState").mockResolvedValue(guardFixture());
});
afterEach(() => vi.restoreAllMocks());

describe("configuration protection", () => {
  it.each([
    { enabled: true, phase: "managed" as const },
    { enabled: false, phase: "unmanaged" as const },
    { enabled: false, phase: "pending_release" as const },
  ])(
    "shows read-detected external changes in $phase without automatically applying or preparing them",
    async (entry) => {
      const state = {
        ...guardFixture(),
        files: [
          { ...guardFixture().files[0], revision: "external-file-hash:42" },
        ],
        pendingChanges: [externalChangeFixture()],
      };
      vi.mocked(configGuardApi.getState).mockResolvedValue(state);
      const prepare = vi.spyOn(configGuardApi, "preview");
      const restore = vi.spyOn(configGuardApi, "previewRestore");
      const save = vi.spyOn(configGuardApi, "setProtection");
      const apply = vi
        .spyOn(configGuardApi, "apply")
        .mockResolvedValue(guardFixture());
      renderSafetyUi(
        <ConfigGuardPanel appId="codex" />,
        managementFixture({ codex: entry }),
      );

      const pending = await screen.findByRole("button", { name: /1 changes/ });
      expect(prepare).not.toHaveBeenCalled();
      expect(restore).not.toHaveBeenCalled();
      expect(save).not.toHaveBeenCalled();
      expect(apply).not.toHaveBeenCalled();
      fireEvent.click(pending);
      const diff = screen.getByRole("region", {
        name: "Configuration differences",
      });
      expect(
        within(diff).getByText("Configuration changed outside ccs"),
      ).toBeVisible();
      expect(within(diff).getAllByText("string (redacted)")).toHaveLength(2);
      const applyButton = within(diff).getByRole("button", {
        name: "Apply ccs changes",
      });
      if (entry.enabled) expect(applyButton).toBeEnabled();
      else expect(applyButton).toBeDisabled();

      fireEvent.click(within(diff).getByRole("button", { name: "Keep local" }));
      expect(apply).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
      await waitFor(() =>
        expect(apply).toHaveBeenCalledWith(
          "backend-external-change-preview",
          "keep_local",
        ),
      );
      expect(apply).toHaveBeenCalledTimes(1);
      expect(prepare).not.toHaveBeenCalled();
      expect(restore).not.toHaveBeenCalled();
      expect(save).not.toHaveBeenCalled();
    },
  );

  it.each(["missing:42", `${"a".repeat(64)}:42`])(
    "passes the opaque protection revision %s unchanged without interpreting it",
    async (revision) => {
      const state = {
        ...guardFixture(),
        files: [{ ...guardFixture().files[0], revision }],
      };
      vi.mocked(configGuardApi.getState).mockResolvedValue(state);
      const save = vi
        .spyOn(configGuardApi, "setProtection")
        .mockResolvedValue(state);
      renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
      fireEvent.click(
        await screen.findByRole("button", { name: "Save protection rules" }),
      );
      await waitFor(() =>
        expect(save).toHaveBeenCalledWith(
          "codex",
          "backend-file-id",
          ["model_catalog_json"],
          false,
          revision,
        ),
      );
    },
  );

  it("saves field ownership with backend-issued file ID and expected revision, never a writable path", async () => {
    const save = vi
      .spyOn(configGuardApi, "setProtection")
      .mockResolvedValue(guardFixture());
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    const paths = await screen.findByLabelText(
      "User-maintained fields or blocks",
    );
    fireEvent.change(paths, {
      target: {
        value:
          "model_catalog_json\nmodels.input_modalities\nmodel_catalog_json\n",
      },
    });
    fireEvent.click(
      screen.getByRole("checkbox", { name: "User maintains the entire file" }),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Save protection rules" }),
    );
    await waitFor(() =>
      expect(save).toHaveBeenCalledWith(
        "codex",
        "backend-file-id",
        ["model_catalog_json", "models.input_modalities"],
        true,
        "file-1",
      ),
    );
    expect(
      screen.queryByDisplayValue("D:/isolated/codex/config.toml"),
    ).not.toBeInTheDocument();
  });

  it("shows three-way conflict evidence and requires explicit confirmation to apply", async () => {
    vi.spyOn(configGuardApi, "preview").mockResolvedValue(previewFixture());
    const apply = vi
      .spyOn(configGuardApi, "apply")
      .mockResolvedValue(guardFixture());
    renderSafetyUi(
      <ConfigGuardPanel appId="codex" />,
      managementFixture({ codex: { phase: "pending_review" } }),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Check differences" }),
    );
    expect(await screen.findByText("local-model")).toBeVisible();
    expect(screen.getByText("db-model")).toBeVisible();
    expect(screen.getByText("model has an external edit")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Apply ccs changes" }));
    expect(apply).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    await waitFor(() =>
      expect(apply).toHaveBeenCalledWith("backend-preview", "apply_ccs"),
    );
  });

  it("requires a new preview after protection rules change, even when the file revision is unchanged", async () => {
    vi.spyOn(configGuardApi, "preview").mockResolvedValue(previewFixture());
    const apply = vi.spyOn(configGuardApi, "apply");
    vi.spyOn(configGuardApi, "setProtection").mockResolvedValue({
      ...guardFixture(),
      files: [{ ...guardFixture().files[0], protectedPaths: ["model"] }],
    });
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    fireEvent.click(
      await screen.findByRole("button", { name: "Check differences" }),
    );
    await screen.findByRole("button", { name: "Apply ccs changes" });
    fireEvent.change(
      screen.getByLabelText("User-maintained fields or blocks"),
      { target: { value: "model" } },
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Save protection rules" }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("region", { name: "Configuration differences" }),
      ).not.toBeInTheDocument(),
    );
    expect(apply).not.toHaveBeenCalled();
  });

  it("keeps a protection-rule version error visible after the changed file refreshes", async () => {
    vi.spyOn(configGuardApi, "setProtection").mockRejectedValue(
      new Error("File changed before saving rules"),
    );
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    await screen.findByRole("button", { name: "Save protection rules" });
    vi.mocked(configGuardApi.getState).mockResolvedValue({
      ...guardFixture(),
      files: [{ ...guardFixture().files[0], revision: "file-2" }],
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Save protection rules" }),
    );
    await waitFor(() =>
      expect(configGuardApi.getState).toHaveBeenCalledTimes(2),
    );
    await waitFor(() =>
      expect(
        screen.getAllByText("File changed before saving rules"),
      ).toHaveLength(1),
    );
  });

  it("allows keeping local data but not writing ccs changes for an unmanaged app", async () => {
    vi.spyOn(configGuardApi, "preview").mockResolvedValue(previewFixture());
    const apply = vi
      .spyOn(configGuardApi, "apply")
      .mockResolvedValue(guardFixture());
    renderSafetyUi(
      <ConfigGuardPanel appId="codex" />,
      managementFixture({ codex: { enabled: false, phase: "unmanaged" } }),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Check differences" }),
    );
    expect(
      await screen.findByRole("button", { name: "Apply ccs changes" }),
    ).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Keep local" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    await waitFor(() =>
      expect(apply).toHaveBeenCalledWith("backend-preview", "keep_local"),
    );
  });

  it("discards a failed version preview and requires a fresh check", async () => {
    vi.spyOn(configGuardApi, "preview").mockResolvedValue(previewFixture());
    const apply = vi
      .spyOn(configGuardApi, "apply")
      .mockRejectedValue(new Error("File changed since preview"));
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    fireEvent.click(
      await screen.findByRole("button", { name: "Check differences" }),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Apply ccs changes" }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    expect(await screen.findByText("File changed since preview")).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Apply ccs changes" }),
    ).not.toBeInTheDocument();
    expect(apply).toHaveBeenCalledTimes(1);
  });

  it("keeps reenabled management pending until every file baseline is explicitly confirmed", async () => {
    const first = { ...guardFixture().files[0], hasBaseline: false };
    const second = {
      ...first,
      id: "catalog-id",
      path: "D:/isolated/codex/catalog.json",
      format: "json",
    };
    const initial = { ...guardFixture(), files: [first, second] };
    const pending = managementFixture({ codex: { phase: "pending_review" } });
    vi.spyOn(appManagementApi, "getState").mockResolvedValue(pending);
    vi.mocked(configGuardApi.getState).mockResolvedValue(initial);
    vi.spyOn(configGuardApi, "preview").mockImplementation(
      async (_app, fileId) => ({
        ...previewFixture(),
        id: `preview-${fileId}`,
        path: fileId === first.id ? first.path : second.path,
        conflicts: [],
        changes: [],
      }),
    );
    const apply = vi
      .spyOn(configGuardApi, "apply")
      .mockResolvedValueOnce({
        ...initial,
        files: [{ ...first, hasBaseline: true }, second],
      })
      .mockResolvedValueOnce({
        ...initial,
        files: [
          { ...first, hasBaseline: true },
          { ...second, hasBaseline: true },
        ],
      });
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, pending);
    expect(
      await screen.findByText(/Review each registered file/),
    ).toBeVisible();
    fireEvent.click(
      (await screen.findAllByRole("button", { name: "Check differences" }))[0],
    );
    fireEvent.click(await screen.findByRole("button", { name: "Keep local" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    await waitFor(() =>
      expect(apply).toHaveBeenCalledWith(`preview-${first.id}`, "keep_local"),
    );
    expect(
      screen.getByText("Enabled; configuration review pending"),
    ).toBeVisible();
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    vi.mocked(appManagementApi.getState).mockResolvedValue(managementFixture());
    fireEvent.click(
      screen.getAllByRole("button", { name: "Check differences" })[1],
    );
    fireEvent.click(await screen.findByRole("button", { name: "Keep local" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    await waitFor(() =>
      expect(apply).toHaveBeenCalledWith(`preview-${second.id}`, "keep_local"),
    );
    await waitFor(() =>
      expect(
        screen.queryByText("Enabled; configuration review pending"),
      ).not.toBeInTheDocument(),
    );
    expect(apply).toHaveBeenCalledTimes(2);
  });

  it("revokes a pending write confirmation when management becomes stopped", async () => {
    vi.spyOn(configGuardApi, "preview").mockResolvedValue(previewFixture());
    const apply = vi.spyOn(configGuardApi, "apply");
    const { queryClient } = renderSafetyUi(
      <ConfigGuardPanel appId="codex" />,
      managementFixture(),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Check differences" }),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Apply ccs changes" }),
    );
    act(() =>
      queryClient.setQueryData(
        ["appManagement"],
        managementFixture({
          codex: { enabled: false, phase: "pending_release" },
        }),
      ),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Confirm" })).toBeDisabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    expect(apply).not.toHaveBeenCalled();
  });
});

describe("local backup metadata and safe restoration", () => {
  beforeEach(() => {
    vi.mocked(configGuardApi.getState).mockResolvedValue(backupStateFixture());
  });

  it("accepts legacy states without backup or audit arrays", async () => {
    vi.mocked(configGuardApi.getState).mockResolvedValue(guardFixture());
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    expect(
      await screen.findByText("No pre-write backups are available."),
    ).toBeVisible();
    const audit = screen.getByText("Change source audit").closest("details")!;
    expect(audit).not.toHaveAttribute("open");
    expect(
      within(audit).getByText("No local change audit records are available."),
    ).not.toBeVisible();
  });

  it("does not turn an active restore into a normal preview when a state refresh detects external changes", async () => {
    vi.spyOn(configGuardApi, "previewRestore").mockResolvedValue(
      restorePreviewFixture(),
    );
    const apply = vi.spyOn(configGuardApi, "apply");
    const { queryClient } = renderSafetyUi(
      <ConfigGuardPanel appId="codex" />,
      managementFixture(),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Preview group restore" }),
    );
    await screen.findByRole("region", { name: "Backup restore preview" });
    act(() =>
      queryClient.setQueryData(["configGuard", "codex"], {
        ...backupStateFixture(),
        files: [
          { ...guardFixture().files[0], revision: "external-file-hash:42" },
        ],
        pendingChanges: [externalChangeFixture()],
      }),
    );
    await screen.findByRole("button", { name: /1 changes/ });
    expect(
      screen.getByRole("region", { name: "Backup restore preview" }),
    ).toBeVisible();
    expect(
      screen.queryByRole("region", { name: "Configuration differences" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Keep local" }),
    ).not.toBeInTheDocument();
    expect(document.body).not.toHaveTextContent("synthetic-current-secret");
    expect(document.body).not.toHaveTextContent("synthetic-backup-secret");
    expect(apply).not.toHaveBeenCalled();
  });

  it("previews all linked metadata and submits one backend-issued group preview only after confirmation", async () => {
    const preview = vi
      .spyOn(configGuardApi, "previewRestore")
      .mockResolvedValue(restorePreviewFixture());
    const ordinaryPreview = vi.spyOn(configGuardApi, "preview");
    const apply = vi
      .spyOn(configGuardApi, "apply")
      .mockResolvedValue(guardFixture());
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    const buttons = await screen.findAllByRole("button", {
      name: "Preview group restore",
    });
    expect(buttons).toHaveLength(1);
    expect(screen.getByText("Backup group · files: 2")).toBeVisible();
    fireEvent.click(buttons[0]);
    const diff = await screen.findByRole("region", {
      name: "Backup restore preview",
    });
    expect(preview).toHaveBeenCalledTimes(1);
    expect(preview).toHaveBeenCalledWith("codex", "backup-config");
    expect(ordinaryPreview).not.toHaveBeenCalled();
    expect(
      within(diff).getByText("D:/isolated/codex/config.toml"),
    ).toBeVisible();
    expect(within(diff).getByText("D:/isolated/codex/auth.json")).toBeVisible();
    expect(document.body).not.toHaveTextContent("synthetic-current-secret");
    expect(document.body).not.toHaveTextContent("synthetic-backup-secret");
    expect(
      screen.queryByRole("button", { name: "Keep local" }),
    ).not.toBeInTheDocument();
    expect(apply).not.toHaveBeenCalled();

    fireEvent.click(
      screen.getByRole("button", { name: "Restore linked group" }),
    );
    const dialog = screen.getByRole("dialog", { name: "Restore linked group" });
    expect(
      within(dialog).getByText(/checks every file and protection rule again/),
    ).toBeVisible();
    expect(apply).not.toHaveBeenCalled();
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm" }));
    await waitFor(() =>
      expect(apply).toHaveBeenCalledWith(
        "backend-group-restore-preview",
        "apply_ccs",
      ),
    );
    expect(apply).toHaveBeenCalledTimes(1);
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(
      screen.queryByRole("region", { name: "Backup restore preview" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByText("No pre-write backups are available."),
    ).toBeVisible();
  });

  it("does not combine unrelated legacy backups with empty group identifiers", async () => {
    const state = backupStateFixture();
    state.backups = state.backups!.map((backup) => ({
      ...backup,
      groupId: "",
    }));
    vi.mocked(configGuardApi.getState).mockResolvedValue(state);
    const preview = vi
      .spyOn(configGuardApi, "previewRestore")
      .mockResolvedValue(restorePreviewFixture());
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    const buttons = await screen.findAllByRole("button", {
      name: "Preview group restore",
    });
    expect(buttons).toHaveLength(2);
    fireEvent.click(buttons[0]);
    const diff = await screen.findByRole("region", {
      name: "Backup restore preview",
    });
    expect(
      within(diff).getByText("D:/isolated/codex/config.toml"),
    ).toBeVisible();
    expect(
      within(diff).queryByText("D:/isolated/codex/auth.json"),
    ).not.toBeInTheDocument();
    expect(preview).toHaveBeenCalledTimes(1);
    expect(preview).toHaveBeenCalledWith("codex", "backup-config");
  });

  it.each([
    { enabled: false, phase: "unmanaged" as const },
    { enabled: false, phase: "pending_release" as const },
    { enabled: true, phase: "pending_review" as const },
  ])(
    "keeps backups readable but disables restoration in $phase",
    async (entry) => {
      const preview = vi.spyOn(configGuardApi, "previewRestore");
      const apply = vi.spyOn(configGuardApi, "apply");
      renderSafetyUi(
        <ConfigGuardPanel appId="codex" />,
        managementFixture({ codex: entry }),
      );
      const button = await screen.findByRole("button", {
        name: "Preview group restore",
      });
      expect(button).toBeDisabled();
      expect(
        screen.getByText(/Restoring requires fully enabled management/),
      ).toBeVisible();
      expect(screen.getByText("D:/isolated/codex/auth.json")).toBeVisible();
      fireEvent.click(button);
      expect(preview).not.toHaveBeenCalled();
      expect(apply).not.toHaveBeenCalled();
    },
  );

  it("does not enable restoration when management state cannot be read", async () => {
    vi.spyOn(appManagementApi, "getState").mockRejectedValue(
      new Error("Management unavailable"),
    );
    const preview = vi.spyOn(configGuardApi, "previewRestore");
    renderSafetyUi(<ConfigGuardPanel appId="codex" />);
    const button = await screen.findByRole("button", {
      name: "Preview group restore",
    });
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(preview).not.toHaveBeenCalled();
  });

  it("revokes a restore confirmation if management is stopped while the dialog is open", async () => {
    vi.spyOn(configGuardApi, "previewRestore").mockResolvedValue(
      restorePreviewFixture(),
    );
    const apply = vi.spyOn(configGuardApi, "apply");
    const { queryClient } = renderSafetyUi(
      <ConfigGuardPanel appId="codex" />,
      managementFixture(),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Preview group restore" }),
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Restore linked group" }),
    );
    act(() =>
      queryClient.setQueryData(
        ["appManagement"],
        managementFixture({
          codex: { enabled: false, phase: "pending_release" },
        }),
      ),
    );
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Confirm" })).toBeDisabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    expect(apply).not.toHaveBeenCalled();
  });

  it("does not offer force-restoring a conflicting backup preview", async () => {
    vi.spyOn(configGuardApi, "previewRestore").mockResolvedValue({
      ...restorePreviewFixture(),
      conflicts: ["A linked file has an external edit"],
    });
    const apply = vi.spyOn(configGuardApi, "apply");
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    fireEvent.click(
      await screen.findByRole("button", { name: "Preview group restore" }),
    );
    expect(
      await screen.findByText("A linked file has an external edit"),
    ).toBeVisible();
    const button = screen.getByRole("button", { name: "Restore linked group" });
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(apply).not.toHaveBeenCalled();
  });

  it.each(["preview", "commit"] as const)(
    "refreshes and discards stale restoration after a %s version rejection, without retrying it",
    async (stage) => {
      const preview = vi
        .spyOn(configGuardApi, "previewRestore")
        .mockResolvedValue(restorePreviewFixture());
      const apply = vi.spyOn(configGuardApi, "apply");
      const rejection = new Error("Configuration changed after the backup");
      if (stage === "preview") preview.mockRejectedValue(rejection);
      else apply.mockRejectedValue(rejection);
      renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
      const button = await screen.findByRole("button", {
        name: "Preview group restore",
      });
      vi.mocked(configGuardApi.getState).mockResolvedValue(guardFixture());
      fireEvent.click(button);
      if (stage === "commit") {
        fireEvent.click(
          await screen.findByRole("button", { name: "Restore linked group" }),
        );
        fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
      }
      expect(
        await screen.findByText(/Configuration changed after the backup/),
      ).toBeVisible();
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Restoration will not be retried automatically.",
      );
      await waitFor(() =>
        expect(configGuardApi.getState).toHaveBeenCalledTimes(2),
      );
      expect(
        screen.queryByRole("region", { name: "Backup restore preview" }),
      ).not.toBeInTheDocument();
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      expect(preview).toHaveBeenCalledTimes(1);
      expect(apply).toHaveBeenCalledTimes(stage === "commit" ? 1 : 0);
      fireEvent.click(screen.getByRole("button", { name: "Retry" }));
      await waitFor(() =>
        expect(configGuardApi.getState).toHaveBeenCalledTimes(3),
      );
      expect(preview).toHaveBeenCalledTimes(1);
      expect(apply).toHaveBeenCalledTimes(stage === "commit" ? 1 : 0);
    },
  );

  it.each(["file", "pending"] as const)(
    "clears the restore intent when selecting an ordinary %s preview",
    async (source) => {
      const state = {
        ...backupStateFixture(),
        pendingChanges: [previewFixture()],
      };
      vi.mocked(configGuardApi.getState).mockResolvedValue(state);
      vi.spyOn(configGuardApi, "previewRestore").mockResolvedValue(
        restorePreviewFixture(),
      );
      vi.spyOn(configGuardApi, "preview").mockResolvedValue(previewFixture());
      renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
      fireEvent.click(
        await screen.findByRole("button", { name: "Preview group restore" }),
      );
      await screen.findByRole("region", { name: "Backup restore preview" });
      fireEvent.click(
        screen.getByRole("button", {
          name: source === "file" ? "Check differences" : /1 changes/,
        }),
      );
      const diff = await screen.findByRole("region", {
        name: "Configuration differences",
      });
      expect(within(diff).getByText("local-model")).toBeVisible();
      expect(
        within(diff).getByRole("button", { name: "Apply ccs changes" }),
      ).toBeEnabled();
      expect(
        within(diff).getByRole("button", { name: "Keep local" }),
      ).toBeEnabled();
      expect(
        screen.queryByRole("button", { name: "Restore linked group" }),
      ).not.toBeInTheDocument();
      expect(document.body).not.toHaveTextContent("synthetic-backup-secret");
    },
  );

  it("discards a prepared restoration when protection rules change", async () => {
    vi.spyOn(configGuardApi, "previewRestore").mockResolvedValue(
      restorePreviewFixture(),
    );
    vi.spyOn(configGuardApi, "setProtection").mockResolvedValue(
      backupStateFixture(),
    );
    const apply = vi.spyOn(configGuardApi, "apply");
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    fireEvent.click(
      await screen.findByRole("button", { name: "Preview group restore" }),
    );
    await screen.findByRole("region", { name: "Backup restore preview" });
    fireEvent.click(
      screen.getByRole("button", { name: "Save protection rules" }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("region", { name: "Backup restore preview" }),
      ).not.toBeInTheDocument(),
    );
    expect(apply).not.toHaveBeenCalled();
  });

  it("starts the audit collapsed and displays localized results and metadata without raw backup data", async () => {
    const history: GuardAudit[] = (
      ["applied", "conflict", "failed", "kept_local"] as const
    ).map((result, index) => ({
      id: `audit-${index}`,
      createdAt: "2026-09-06T08:00:00Z",
      source: [
        "provider_switch",
        "native_configuration",
        "backup_restore",
        "custom_operation",
      ][index],
      result,
      paths: ["D:/isolated/codex/auth.json"],
      fields: ["OPENAI_API_KEY"],
    }));
    vi.mocked(configGuardApi.getState).mockResolvedValue({
      ...backupStateFixture(),
      history,
    });
    renderSafetyUi(<ConfigGuardPanel appId="codex" />, managementFixture());
    const summary = await screen.findByText("Change source audit");
    const audit = summary.closest("details")!;
    expect(audit).not.toHaveAttribute("open");
    expect(within(audit).getByText("Applied")).not.toBeVisible();
    fireEvent.click(summary);
    expect(audit).toHaveAttribute("open");
    for (const result of ["Applied", "Conflict", "Failed", "Kept local"]) {
      expect(within(audit).getByText(result)).toBeVisible();
    }
    for (const source of [
      "Provider switch",
      "Native configuration",
      "Backup restore",
      "custom_operation",
    ]) {
      expect(within(audit).getByText(`Source: ${source}`)).toBeVisible();
    }
    expect(
      within(audit).getAllByText("Files: D:/isolated/codex/auth.json"),
    ).toHaveLength(4);
    expect(
      within(audit).getAllByText("Fields or blocks: OPENAI_API_KEY"),
    ).toHaveLength(4);
    expect(document.body).not.toHaveTextContent("synthetic-backup-secret");
  });
});
