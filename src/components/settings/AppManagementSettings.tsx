import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Loader2, ShieldCheck } from "lucide-react";
import { APP_IDS, APP_ICON_MAP, getAppLabel } from "@/config/appConfig";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ConfigGuardPanel } from "@/components/management/ConfigGuardPanel";
import {
  appManagementApi,
  type AppManagementPreview,
} from "@/lib/api/appManagement";
import {
  appManagementKeys,
  useAppManagementState,
} from "@/lib/query/appManagement";
import type { AppId } from "@/lib/api/types";
import { extractErrorMessage } from "@/utils/errorUtils";

export function AppManagementSettings() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const state = useAppManagementState();
  const [preview, setPreview] = useState<AppManagementPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [guardApp, setGuardApp] = useState<AppId | null>(null);

  const prepare = async (appId: AppId, enabled: boolean) => {
    setBusy(true);
    setError("");
    setPreview(null);
    try {
      setPreview(await appManagementApi.preview(appId, enabled));
    } catch (cause) {
      setError(extractErrorMessage(cause));
      void state.refetch();
    } finally {
      setBusy(false);
    }
  };

  const apply = async () => {
    if (!preview || busy) return;
    setBusy(true);
    setError("");
    try {
      const next = await appManagementApi.apply(preview.id);
      // A read started before this transaction must not restore stale write
      // permission in the UI after the authoritative state was changed.
      await queryClient.cancelQueries({ queryKey: appManagementKeys.state });
      queryClient.setQueryData(appManagementKeys.state, next);
      void queryClient.invalidateQueries({
        queryKey: ["configGuard", preview.appId],
      });
      void queryClient.invalidateQueries({ queryKey: ["proxy"] });
      setPreview(null);
    } catch (cause) {
      setError(extractErrorMessage(cause));
      // A failed release may have safely stopped ordinary writes already.
      // Always read the authoritative transition state, never flip back locally.
      setPreview(null);
      void state.refetch();
    } finally {
      setBusy(false);
    }
  };

  const stale = Boolean(
    preview && state.data && preview.revision !== state.data.revision,
  );

  return (
    <section className="space-y-3" aria-label={t("appManagement.title")}>
      <header className="space-y-1">
        <h3 className="flex items-center gap-2 text-sm font-medium">
          <ShieldCheck className="h-4 w-4" />
          {t("appManagement.title")}
        </h3>
        <p className="text-xs text-muted-foreground">
          {t("appManagement.description")}
        </p>
      </header>
      {(error || state.isError) && (
        <div
          role="alert"
          className="rounded-md border border-destructive/30 p-3 text-sm"
        >
          <p>{error || t("appManagement.unavailable")}</p>
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              setError("");
              void state.refetch();
            }}
          >
            {t("common.retry")}
          </Button>
        </div>
      )}
      <div className="divide-y rounded-lg border border-border-default">
        {APP_IDS.map((appId) => {
          const app = state.data?.apps.find((entry) => entry.appId === appId);
          return (
            <div key={appId} className="flex flex-wrap items-center gap-3 p-3">
              <div className="flex min-w-36 flex-1 items-start gap-2">
                <span className="mt-0.5">{APP_ICON_MAP[appId].icon}</span>
                <div className="space-y-1">
                  <p className="text-sm font-medium">{getAppLabel(appId)}</p>
                  <p className="text-xs text-muted-foreground">
                    {t(
                      app
                        ? `appManagement.phases.${app.phase}`
                        : state.isError
                          ? "appManagement.unavailable"
                          : "appManagement.loading",
                    )}
                  </p>
                  {app?.message && (
                    <p className="max-w-lg break-words text-xs text-amber-700 dark:text-amber-300">
                      {app.message}
                    </p>
                  )}
                </div>
              </div>
              {app?.phase === "pending_release" && (
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy || !state.isSuccess}
                  onClick={() => void prepare(appId, false)}
                >
                  {t("appManagement.retryRelease")}
                </Button>
              )}
              <Button
                size="sm"
                variant="ghost"
                onClick={() => setGuardApp(appId)}
              >
                {t("configGuard.title")}
              </Button>
              <Switch
                checked={app?.enabled ?? false}
                aria-label={t("appManagement.toggle", {
                  app: getAppLabel(appId),
                })}
                disabled={busy || !state.isSuccess || !app}
                onCheckedChange={(enabled) => void prepare(appId, enabled)}
              />
            </div>
          );
        })}
      </div>
      <p className="text-xs text-muted-foreground">
        {t("appManagement.localOnly")}
      </p>
      {busy && !preview && (
        <p role="status" className="flex gap-2 text-sm">
          <Loader2 className="h-4 w-4 animate-spin" />
          {t("appManagement.preparing")}
        </p>
      )}
      <Dialog
        open={Boolean(preview)}
        onOpenChange={(open) => {
          if (!open && !busy) setPreview(null);
        }}
      >
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>
              {t("appManagement.previewTitle", {
                app: preview ? getAppLabel(preview.appId) : "",
              })}
            </DialogTitle>
            <DialogDescription>
              {t(
                preview?.enabled
                  ? "appManagement.enableExplanation"
                  : "appManagement.disableExplanation",
              )}
            </DialogDescription>
          </DialogHeader>
          <div className="max-h-[55vh] space-y-3 overflow-y-auto px-6 py-4 text-sm">
            {preview?.files.length === 0 && <p>{t("appManagement.noFiles")}</p>}
            {preview?.files.map((file, index) => (
              <div key={`${file.path}-${index}`} className="rounded border p-2">
                <p className="break-all font-mono text-xs">{file.path}</p>
                <p>{file.action}</p>
              </div>
            ))}
            {preview?.warnings.map((warning, index) => (
              <p key={index} className="text-amber-700 dark:text-amber-300">
                {warning}
              </p>
            ))}
            {Boolean(preview?.conflicts.length) && (
              <div
                role="alert"
                className="space-y-1 rounded border border-amber-500/40 p-3"
              >
                <p className="font-medium">
                  {t(
                    preview?.enabled
                      ? "appManagement.conflictsBlockEnable"
                      : "appManagement.conflictsStopWrites",
                  )}
                </p>
                {preview?.conflicts.map((conflict, index) => (
                  <p key={index}>{conflict}</p>
                ))}
              </div>
            )}
            {stale && <p role="alert">{t("appManagement.stalePreview")}</p>}
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              disabled={busy}
              onClick={() => setPreview(null)}
            >
              {t("common.cancel")}
            </Button>
            {stale ? (
              <Button
                disabled={busy}
                onClick={() =>
                  preview && void prepare(preview.appId, preview.enabled)
                }
              >
                {t("appManagement.previewAgain")}
              </Button>
            ) : (
              <Button
                disabled={
                  busy ||
                  !state.isSuccess ||
                  Boolean(preview?.enabled && preview.conflicts.length)
                }
                onClick={() => void apply()}
              >
                {busy && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                {t(
                  preview?.enabled
                    ? "appManagement.confirmEnable"
                    : preview?.conflicts.length
                      ? "appManagement.confirmStopPending"
                      : "appManagement.confirmDisable",
                )}
              </Button>
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <Dialog
        open={Boolean(guardApp)}
        onOpenChange={(open) => {
          if (!open) setGuardApp(null);
        }}
      >
        <DialogContent className="max-w-3xl">
          <DialogHeader>
            <DialogTitle>
              {t("configGuard.appTitle", {
                app: guardApp ? getAppLabel(guardApp) : "",
              })}
            </DialogTitle>
            <DialogDescription>
              {t("configGuard.description")}
            </DialogDescription>
          </DialogHeader>
          <div className="max-h-[65vh] overflow-y-auto px-6 py-4">
            {guardApp && <ConfigGuardPanel key={guardApp} appId={guardApp} />}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setGuardApp(null)}>
              {t("common.close")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}
