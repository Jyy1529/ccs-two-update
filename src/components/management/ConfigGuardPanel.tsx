import { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Textarea } from "@/components/ui/textarea";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { AppManagementNotice } from "./AppManagementNotice";
import {
  configGuardApi,
  type ConfigChangePreview,
  type ConfigGuardFile,
  type GuardBackup,
} from "@/lib/api/appManagement";
import {
  appManagementKeys,
  useAppManagement,
  useConfigGuard,
} from "@/lib/query/appManagement";
import type { AppId } from "@/lib/api/types";
import { extractErrorMessage } from "@/utils/errorUtils";

export function ConfigGuardPanel({ appId }: { appId: AppId }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const query = useConfigGuard(appId);
  const management = useAppManagement(appId);
  const [review, setReview] = useState<{
    preview: ConfigChangePreview;
    backups?: GuardBackup[];
  } | null>(null);
  const [resolution, setResolution] = useState<
    "keep_local" | "apply_ccs" | null
  >(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const preview = review?.preview;
  const restoring = review?.backups !== undefined;
  const canApplyPreview = Boolean(
    preview &&
      query.isSuccess &&
      (restoring
        ? management.canWrite && preview.conflicts.length === 0
        : management.canReview),
  );
  const canConfirm = Boolean(
    preview &&
      query.isSuccess &&
      management.isSuccess &&
      ((resolution === "keep_local" && !restoring) ||
        (resolution === "apply_ccs" && canApplyPreview)),
  );
  const backupGroups = new Map<string, GuardBackup[]>();
  for (const backup of query.data?.backups ?? []) {
    // Legacy backups without a group must remain independent.
    const key = backup.groupId
      ? `group:${backup.groupId}`
      : `file:${backup.id}`;
    const group = backupGroups.get(key) ?? [];
    group.push(backup);
    backupGroups.set(key, group);
  }

  const prepare = async (id: string, backups?: GuardBackup[]) => {
    if (
      busy ||
      !query.isSuccess ||
      !management.isSuccess ||
      (backups && !management.canWrite)
    )
      return;
    setBusy(true);
    setError("");
    setResolution(null);
    setReview(null);
    try {
      const next = backups
        ? await configGuardApi.previewRestore(appId, id)
        : await configGuardApi.preview(appId, id);
      setReview({ preview: next, backups });
    } catch (cause) {
      setError(
        backups
          ? `${extractErrorMessage(cause)} ${t("configGuard.restoreRefreshHint")}`
          : extractErrorMessage(cause),
      );
      void query.refetch();
    } finally {
      setBusy(false);
    }
  };

  const apply = async () => {
    if (!preview || !resolution || busy || !canConfirm) return;
    setBusy(true);
    setError("");
    try {
      const state = await configGuardApi.apply(preview.id, resolution);
      await queryClient.cancelQueries({
        queryKey: appManagementKeys.guard(appId),
      });
      queryClient.setQueryData(appManagementKeys.guard(appId), state);
      void queryClient.invalidateQueries({ queryKey: appManagementKeys.state });
      setReview(null);
      setResolution(null);
    } catch (cause) {
      setError(
        restoring
          ? `${extractErrorMessage(cause)} ${t("configGuard.restoreRefreshHint")}`
          : extractErrorMessage(cause),
      );
      setResolution(null);
      setReview(null);
      void query.refetch();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-4">
      <AppManagementNotice appId={appId} />
      {management.entry?.phase === "pending_review" && (
        <p className="text-sm">{t("configGuard.reviewHint")}</p>
      )}
      <p className="text-xs text-muted-foreground">
        {t("configGuard.pathBoundary")}
      </p>
      {query.isPending && <p role="status">{t("common.loading")}</p>}
      {(error || query.isError) && (
        <div role="alert" className="space-y-2 text-sm text-destructive">
          <p>{error || t("configGuard.loadFailed")}</p>
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              setError("");
              setReview(null);
              setResolution(null);
              void query.refetch();
            }}
          >
            {t("common.retry")}
          </Button>
        </div>
      )}
      {query.data?.files.length === 0 && (
        <p className="text-sm text-muted-foreground">
          {t("configGuard.noFiles")}
        </p>
      )}
      {query.data?.files.map((file) => (
        <ProtectionEditor
          key={`${file.id}:${file.revision}`}
          appId={appId}
          file={file}
          disabled={busy || !query.isSuccess || !management.isSuccess}
          onPreview={() => void prepare(file.id)}
          onSaved={() => {
            setReview(null);
            setResolution(null);
            setError("");
          }}
          onFailure={setError}
        />
      ))}
      {Boolean(query.data?.pendingChanges.length) && (
        <section className="space-y-2">
          <h4 className="text-sm font-medium">
            {t("configGuard.pendingChanges")}
          </h4>
          {query.data?.pendingChanges.map((change) => (
            <Button
              key={change.id}
              variant="outline"
              className="h-auto w-full justify-start whitespace-normal break-all text-left"
              onClick={() => {
                setReview({ preview: change });
                setResolution(null);
                setError("");
              }}
              disabled={busy}
            >
              {change.path} ·{" "}
              {t("configGuard.changeCount", { count: change.changes.length })}
            </Button>
          ))}
        </section>
      )}
      {query.isSuccess && (
        <section
          className="space-y-2"
          aria-label={t("configGuard.backupsTitle")}
        >
          <h4 className="text-sm font-medium">
            {t("configGuard.backupsTitle")}
          </h4>
          <p className="text-xs text-muted-foreground">
            {t("configGuard.backupsHint")}
          </p>
          {!backupGroups.size && (
            <p className="text-xs text-muted-foreground">
              {t("configGuard.noBackups")}
            </p>
          )}
          {backupGroups.size > 0 && !management.canWrite && (
            <p className="text-xs text-muted-foreground">
              {t("configGuard.restoreManagedOnly")}
            </p>
          )}
          <div className="max-h-80 space-y-2 overflow-auto">
            {[...backupGroups.entries()].map(([groupId, backups]) => (
              <article
                key={groupId}
                className="space-y-2 rounded-lg border border-border-default p-3"
              >
                <div className="flex flex-wrap items-center justify-between gap-2 text-xs">
                  <h5 className="font-medium">
                    {t("configGuard.backupGroup", { count: backups.length })}
                  </h5>
                  <time dateTime={backups[0].createdAt || undefined}>
                    {backups[0].createdAt || "—"}
                  </time>
                </div>
                <BackupFiles backups={backups} />
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy || !query.isSuccess || !management.canWrite}
                  onClick={() => void prepare(backups[0].id, backups)}
                >
                  {t("configGuard.previewRestore")}
                </Button>
              </article>
            ))}
          </div>
        </section>
      )}
      {query.isSuccess && (
        <details className="rounded-lg border border-border-default p-3 text-xs">
          <summary className="cursor-pointer font-medium">
            {t("configGuard.auditTitle")}
          </summary>
          {(query.data?.history ?? []).length === 0 ? (
            <p className="mt-2 text-muted-foreground">
              {t("configGuard.noAudit")}
            </p>
          ) : (
            <ol className="mt-3 max-h-72 space-y-3 overflow-auto">
              {(query.data?.history ?? []).map((entry) => (
                <li key={entry.id} className="space-y-1">
                  <p className="flex flex-wrap gap-2 font-medium">
                    <time dateTime={entry.createdAt || undefined}>
                      {entry.createdAt || "—"}
                    </time>
                    <span>{t(`configGuard.auditResults.${entry.result}`)}</span>
                  </p>
                  <p className="break-all">
                    {t("configGuard.source")}:{" "}
                    {t(`configGuard.sources.${entry.source}`, {
                      defaultValue: entry.source || "—",
                    })}
                  </p>
                  <p className="break-all">
                    {t("configGuard.affectedFiles")}:{" "}
                    {entry.paths.join(" · ") || "—"}
                  </p>
                  <p className="break-all text-muted-foreground">
                    {t("configGuard.affectedFields")}:{" "}
                    {entry.fields.join(" · ") || "—"}
                  </p>
                </li>
              ))}
            </ol>
          )}
        </details>
      )}
      {preview && (
        <section
          className="space-y-3 rounded-lg border border-border-default p-3"
          aria-label={t(
            restoring
              ? "configGuard.restoreDiffTitle"
              : "configGuard.diffTitle",
          )}
        >
          <h4 className="break-all text-sm font-medium">
            {restoring ? t("configGuard.restoreDiffTitle") : preview.path}
          </h4>
          {review?.backups && (
            <>
              <p className="text-xs text-muted-foreground">
                {t("configGuard.restorePrivacyHint")}
              </p>
              <BackupFiles backups={review.backups} />
            </>
          )}
          {preview.conflicts.map((conflict, index) => (
            <p
              role="alert"
              key={index}
              className="break-words text-xs text-amber-700 dark:text-amber-300"
            >
              {conflict}
            </p>
          ))}
          {!preview.changes.length && !restoring && (
            <p className="text-sm">{t("configGuard.noChanges")}</p>
          )}
          {preview.changes.map((change, index) => (
            <div
              key={index}
              className="space-y-1 rounded bg-muted/40 p-2 text-xs"
            >
              <p className="break-all font-mono">
                {change.path} · {change.kind}
              </p>
              {!restoring && (
                <div className="grid gap-2 sm:grid-cols-2">
                  <div>
                    <p className="text-muted-foreground">
                      {t("configGuard.before")}
                    </p>
                    <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all">
                      {change.before ?? "—"}
                    </pre>
                  </div>
                  <div>
                    <p className="text-muted-foreground">
                      {t("configGuard.after")}
                    </p>
                    <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all">
                      {change.after ?? "—"}
                    </pre>
                  </div>
                </div>
              )}
            </div>
          ))}
          <p className="text-xs text-muted-foreground">
            {t(
              restoring
                ? "configGuard.restoreRecheckHint"
                : "configGuard.recheckHint",
            )}
          </p>
          <div className="flex flex-wrap gap-2">
            {!restoring && (
              <Button
                size="sm"
                variant="outline"
                disabled={busy || !query.isSuccess || !management.isSuccess}
                onClick={() => setResolution("keep_local")}
              >
                {t("configGuard.keepLocal")}
              </Button>
            )}
            <Button
              size="sm"
              disabled={busy || !canApplyPreview}
              onClick={() => setResolution("apply_ccs")}
            >
              {t(
                restoring ? "configGuard.restoreGroup" : "configGuard.applyCcs",
              )}
            </Button>
          </div>
        </section>
      )}
      <Dialog
        open={Boolean(resolution)}
        onOpenChange={(open) => {
          if (!open && !busy) setResolution(null);
        }}
      >
        <DialogContent zIndex="nested">
          <DialogHeader>
            <DialogTitle>
              {t(
                restoring
                  ? "configGuard.restoreGroup"
                  : resolution === "apply_ccs"
                    ? "configGuard.applyCcs"
                    : "configGuard.keepLocal",
              )}
            </DialogTitle>
            <DialogDescription>
              {t(
                restoring
                  ? "configGuard.confirmRestore"
                  : resolution === "apply_ccs"
                    ? "configGuard.confirmApply"
                    : "configGuard.confirmKeep",
              )}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              disabled={busy}
              onClick={() => setResolution(null)}
            >
              {t("common.cancel")}
            </Button>
            <Button disabled={busy || !canConfirm} onClick={() => void apply()}>
              {t("common.confirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function BackupFiles({ backups }: { backups: GuardBackup[] }) {
  const { t } = useTranslation();
  return (
    <ul className="space-y-2 text-xs">
      {backups.map((backup) => (
        <li key={backup.id} className="space-y-1 break-all">
          <p className="font-mono">{backup.path}</p>
          <p>
            {t("configGuard.source")}:{" "}
            {t(`configGuard.sources.${backup.source}`, {
              defaultValue: backup.source || "—",
            })}
          </p>
          <p className="text-muted-foreground">
            {t("configGuard.affectedFields")}:{" "}
            {backup.fields.join(" · ") || "—"}
          </p>
        </li>
      ))}
    </ul>
  );
}

function ProtectionEditor({
  appId,
  file,
  disabled,
  onPreview,
  onSaved,
  onFailure,
}: {
  appId: AppId;
  file: ConfigGuardFile;
  disabled: boolean;
  onPreview: () => void;
  onSaved: () => void;
  onFailure: (message: string) => void;
}) {
  const { t } = useTranslation();
  const id = useId();
  const queryClient = useQueryClient();
  const [protectFile, setProtectFile] = useState(file.protectFile);
  const [paths, setPaths] = useState(file.protectedPaths.join("\n"));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const save = async () => {
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      const state = await configGuardApi.setProtection(
        appId,
        file.id,
        [
          ...new Set(
            paths
              .split(/\r?\n/)
              .map((path) => path.trim())
              .filter(Boolean),
          ),
        ],
        protectFile,
        file.revision,
      );
      await queryClient.cancelQueries({
        queryKey: appManagementKeys.guard(appId),
      });
      queryClient.setQueryData(appManagementKeys.guard(appId), state);
      setSaved(true);
      onSaved();
    } catch (cause) {
      const message = extractErrorMessage(cause);
      setError(message);
      // Keep the error visible even if refreshing a changed file remounts this
      // editor with a new revision.
      onFailure(message);
      void queryClient.invalidateQueries({
        queryKey: appManagementKeys.guard(appId),
      });
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="space-y-3 rounded-lg border border-border-default p-3">
      <header>
        <p className="break-all font-mono text-xs">{file.path}</p>
        <p className="mt-1 text-xs text-muted-foreground">
          {file.format} ·{" "}
          {t(
            file.hasBaseline
              ? "configGuard.hasBaseline"
              : "configGuard.noBaseline",
          )}
        </p>
      </header>
      <label className="flex items-center gap-2 text-sm">
        <Checkbox
          checked={protectFile}
          onCheckedChange={(value) => {
            setProtectFile(value);
            setSaved(false);
          }}
          disabled={disabled || busy}
        />
        {t("configGuard.protectFile")}
      </label>
      <div className="space-y-1">
        <Label htmlFor={id}>{t("configGuard.protectedPaths")}</Label>
        <Textarea
          id={id}
          value={paths}
          onChange={(event) => {
            setPaths(event.target.value);
            setSaved(false);
          }}
          rows={3}
          disabled={disabled || busy}
          className="font-mono text-xs"
        />
        <p className="text-xs text-muted-foreground">
          {t("configGuard.pathsHint")}
        </p>
      </div>
      {error && (
        <p role="alert" className="text-xs text-destructive">
          {error}
        </p>
      )}
      {saved && (
        <p role="status" className="text-xs">
          {t("configGuard.saved")}
        </p>
      )}
      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          disabled={disabled || busy}
          onClick={() => void save()}
        >
          {t("configGuard.saveRules")}
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={disabled || busy}
          onClick={onPreview}
        >
          {t("configGuard.preview")}
        </Button>
      </div>
    </section>
  );
}
