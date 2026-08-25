import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  AlertCircle,
  ArrowRightLeft,
  Check,
  CheckCircle2,
  Loader2,
  SkipForward,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { Provider } from "@/types";
import type { AppId } from "@/lib/api";
import {
  providersApi,
  type ProviderTransferPreview,
  type ProviderTransferResult,
} from "@/lib/api/providers";
import { APP_ICON_MAP, APP_IDS } from "@/config/appConfig";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

interface ProviderTransferDialogProps {
  open: boolean;
  sourceApp: AppId;
  sourceProvider: Provider;
  onOpenChange: (open: boolean) => void;
}

interface AppOption {
  id: AppId;
  name: string;
  icon: ReactNode;
}

const APP_OPTIONS: AppOption[] = APP_IDS.map((id) => ({
  id,
  name: id === "claude" ? "Claude Code" : APP_ICON_MAP[id].label,
  icon: APP_ICON_MAP[id].icon,
}));

export function ProviderTransferDialog({
  open,
  sourceApp,
  sourceProvider,
  onOpenChange,
}: ProviderTransferDialogProps) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [preview, setPreview] = useState<ProviderTransferPreview | null>(null);
  const [selectedApps, setSelectedApps] = useState<AppId[]>([]);
  const [results, setResults] = useState<ProviderTransferResult[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [isImporting, setIsImporting] = useState(false);
  const [loadError, setLoadError] = useState("");

  const targetApps = useMemo(
    () => APP_OPTIONS.filter((app) => app.id !== sourceApp),
    [sourceApp],
  );

  useEffect(() => {
    if (!open) return;

    let cancelled = false;
    setPreview(null);
    setSelectedApps([]);
    setResults([]);
    setLoadError("");
    setIsLoading(true);

    void providersApi
      .getTransferPreview(sourceApp, sourceProvider.id)
      .then((nextPreview) => {
        if (!cancelled) setPreview(nextPreview);
      })
      .catch((error) => {
        if (!cancelled) {
          setLoadError(error instanceof Error ? error.message : String(error));
        }
      })
      .finally(() => {
        if (!cancelled) setIsLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [open, sourceApp, sourceProvider.id]);

  const toggleTarget = (appId: AppId) => {
    setSelectedApps((current) =>
      current.includes(appId)
        ? current.filter((item) => item !== appId)
        : [...current, appId],
    );
  };

  const handleImport = async () => {
    if (selectedApps.length === 0 || isImporting) return;

    setIsImporting(true);
    setResults([]);
    try {
      const nextResults = await providersApi.transferToApps({
        sourceApp,
        sourceProviderId: sourceProvider.id,
        targetApps: selectedApps,
      });
      setResults(nextResults);

      const createdApps = nextResults
        .filter((result) => result.status === "created")
        .map((result) => result.appId);
      await Promise.all(
        createdApps.map((appId) =>
          queryClient.invalidateQueries({ queryKey: ["providers", appId] }),
        ),
      );

      const failedCount = nextResults.filter(
        (result) => result.status === "failed",
      ).length;
      if (failedCount > 0 && createdApps.length > 0) {
        toast.warning(
          t("providerTransfer.partialSuccess", {
            defaultValue: "部分 Agent 导入完成",
          }),
        );
      } else if (failedCount > 0) {
        toast.error(
          t("providerTransfer.failed", { defaultValue: "供应商导入失败" }),
        );
      } else {
        toast.success(
          t("providerTransfer.success", {
            defaultValue: "供应商已导入",
          }),
        );
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setResults(
        selectedApps.map((appId) => ({
          appId,
          status: "failed" as const,
          message,
        })),
      );
      toast.error(
        t("providerTransfer.failed", { defaultValue: "供应商导入失败" }),
      );
    } finally {
      setIsImporting(false);
    }
  };

  const appName = (appId: AppId) =>
    APP_OPTIONS.find((app) => app.id === appId)?.name ?? appId;

  const fieldValue = (value?: string) => value?.trim() || "-";

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!isImporting) onOpenChange(nextOpen);
      }}
    >
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {t("providerTransfer.title", {
              defaultValue: "导入到其他 Agent",
            })}
          </DialogTitle>
          <DialogDescription>
            {t("providerTransfer.description", {
              defaultValue: "选择接收该供应商的 Agent",
            })}
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 space-y-5 overflow-y-auto px-6 py-5">
          {isLoading ? (
            <div className="flex h-32 items-center justify-center text-muted-foreground">
              <Loader2 className="h-5 w-5 animate-spin" />
            </div>
          ) : loadError ? (
            <div className="flex items-start gap-2 rounded-md border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
              <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
              <span className="break-all">{loadError}</span>
            </div>
          ) : preview ? (
            <>
              <dl className="grid grid-cols-[8rem_minmax(0,1fr)] gap-x-4 gap-y-2 text-sm">
                <dt className="text-muted-foreground">
                  {t("providerTransfer.name", { defaultValue: "供应商名称" })}
                </dt>
                <dd className="min-w-0 break-words font-medium">
                  {fieldValue(preview.name)}
                </dd>
                <dt className="text-muted-foreground">
                  {t("providerTransfer.notes", { defaultValue: "备注" })}
                </dt>
                <dd className="min-w-0 break-words">
                  {fieldValue(preview.notes)}
                </dd>
                <dt className="text-muted-foreground">
                  {t("providerTransfer.website", { defaultValue: "官网链接" })}
                </dt>
                <dd className="min-w-0 break-all">
                  {fieldValue(preview.websiteUrl)}
                </dd>
                <dt className="text-muted-foreground">API Key</dt>
                <dd className="font-mono">
                  {preview.hasApiKey ? "************" : "-"}
                </dd>
                <dt className="text-muted-foreground">
                  {t("providerTransfer.endpoint", {
                    defaultValue: "API 请求地址",
                  })}
                </dt>
                <dd className="min-w-0 break-all">
                  {fieldValue(preview.baseUrl)}
                </dd>
              </dl>

              <div className="space-y-2">
                <div className="text-sm font-medium">
                  {t("providerTransfer.targets", {
                    defaultValue: "目标 Agent",
                  })}
                </div>
                <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
                  {targetApps.map((app) => {
                    const selected = selectedApps.includes(app.id);
                    return (
                      <button
                        key={app.id}
                        type="button"
                        aria-label={app.name}
                        aria-pressed={selected}
                        onClick={() => toggleTarget(app.id)}
                        className={cn(
                          "relative flex h-16 min-w-0 items-center gap-2 rounded-md border px-3 text-left text-sm transition-colors",
                          selected
                            ? "border-blue-500 bg-blue-500/10 text-blue-700 ring-1 ring-blue-500/30 dark:text-blue-300"
                            : "border-border-default bg-background text-muted-foreground hover:border-border-hover hover:bg-muted/50 hover:text-foreground",
                        )}
                      >
                        <span className="inline-flex h-6 w-6 shrink-0 items-center justify-center">
                          {app.icon}
                        </span>
                        <span className="min-w-0 break-words leading-tight">
                          {app.name}
                        </span>
                        {selected && (
                          <span className="absolute right-1.5 top-1.5 flex h-4 w-4 items-center justify-center rounded-full bg-blue-500 text-white">
                            <Check className="h-3 w-3" />
                          </span>
                        )}
                      </button>
                    );
                  })}
                </div>
              </div>

              {results.length > 0 && (
                <div className="space-y-1.5 border-t border-border-default pt-4">
                  {results.map((result) => {
                    const ResultIcon =
                      result.status === "created"
                        ? CheckCircle2
                        : result.status === "skipped"
                          ? SkipForward
                          : AlertCircle;
                    return (
                      <div
                        key={`${result.appId}-${result.status}`}
                        className="flex min-h-8 items-start gap-2 text-sm"
                      >
                        <ResultIcon
                          className={cn(
                            "mt-0.5 h-4 w-4 shrink-0",
                            result.status === "created"
                              ? "text-emerald-500"
                              : result.status === "skipped"
                                ? "text-amber-500"
                                : "text-red-500",
                          )}
                        />
                        <span className="font-medium">
                          {appName(result.appId)}
                        </span>
                        <span className="min-w-0 break-words text-muted-foreground">
                          {result.status === "created"
                            ? result.providerName
                            : result.message ||
                              t("providerTransfer.skipped", {
                                defaultValue: "已跳过",
                              })}
                        </span>
                      </div>
                    );
                  })}
                </div>
              )}
            </>
          ) : null}
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={isImporting}
          >
            {t("common.cancel", { defaultValue: "取消" })}
          </Button>
          <Button
            onClick={() => void handleImport()}
            disabled={
              isLoading ||
              Boolean(loadError) ||
              selectedApps.length === 0 ||
              isImporting
            }
          >
            {isImporting ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <ArrowRightLeft className="h-4 w-4" />
            )}
            {t("providerTransfer.import", { defaultValue: "导入" })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
