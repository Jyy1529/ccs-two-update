import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  CheckCircle2,
  Loader2,
  RefreshCw,
  ShieldCheck,
  Wrench,
} from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { settingsApi } from "@/lib/api";
import type { CodexRepairState, CodexRepairStatus } from "@/lib/api/settings";
import { extractErrorMessage } from "@/utils/errorUtils";

interface CodexRepairSettingsProps {
  enabled: boolean;
  onEnabledChange: (enabled: boolean) => void;
}

export function CodexRepairSettings({
  enabled,
  onEnabledChange,
}: CodexRepairSettingsProps) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<CodexRepairStatus | null>(null);
  const [isChecking, setIsChecking] = useState(false);
  const [isLaunching, setIsLaunching] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);

  const repairRunning = status?.repairRunning === true;

  const checkStatus = useCallback(async () => {
    setIsChecking(true);
    try {
      setStatus(await settingsApi.getCodexRepairStatus());
    } catch (error) {
      console.error("[CodexRepair] Detection failed", error);
      toast.error(
        t("settings.codexRepair.checkFailed", {
          defaultValue: "Codex Desktop 检测失败",
        }),
      );
    } finally {
      setIsChecking(false);
    }
  }, [t]);

  useEffect(() => {
    if (enabled) {
      void checkStatus();
    } else {
      setStatus(null);
    }
  }, [checkStatus, enabled]);

  useEffect(() => {
    if (!enabled || !repairRunning) {
      return;
    }

    const timer = window.setInterval(() => {
      void settingsApi
        .getCodexRepairStatus()
        .then(setStatus)
        .catch((error) => {
          console.error("[CodexRepair] Progress check failed", error);
        });
    }, 5000);

    return () => window.clearInterval(timer);
  }, [enabled, repairRunning]);

  const launchRepair = useCallback(async () => {
    setIsLaunching(true);
    try {
      const result = await settingsApi.launchCodexRepair();
      if (result.started) {
        setStatus((current) =>
          current
            ? { ...current, repairRunning: true, lastRepairError: null }
            : current,
        );
        toast.success(
          t("settings.codexRepair.launchStarted", {
            defaultValue: "已请求管理员权限，请在 UAC 提示中确认",
          }),
        );
        setConfirmOpen(false);
      }
    } catch (error) {
      console.error("[CodexRepair] Launch failed", error);
      const detail = extractErrorMessage(error) || String(error ?? "");
      toast.error(
        t("settings.codexRepair.launchFailed", {
          defaultValue: "无法启动管理员修复",
        }),
        detail ? { description: detail } : undefined,
      );
    } finally {
      setIsLaunching(false);
    }
  }, [t]);

  const stateLabels: Record<CodexRepairState, string> = {
    unsupported: t("settings.codexRepair.state.unsupported", {
      defaultValue: "当前平台不支持",
    }),
    notInstalled: t("settings.codexRepair.state.notInstalled", {
      defaultValue: "未安装 Codex Desktop",
    }),
    healthy: t("settings.codexRepair.state.healthy", {
      defaultValue: "状态健康",
    }),
    needsRepair: t("settings.codexRepair.state.needsRepair", {
      defaultValue: "需要修复",
    }),
    unknown: t("settings.codexRepair.state.unknown", {
      defaultValue: "等待检测",
    }),
  };

  const canRepair =
    status?.platformSupported === true && status.codexInstalled === true;
  const isHealthy = status?.state === "healthy";

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between gap-4 border-b border-border/50 pb-4">
        <div className="flex min-w-0 items-center gap-3">
          <ShieldCheck className="h-5 w-5 shrink-0 text-emerald-500" />
          <div className="min-w-0">
            <p className="text-sm font-medium">
              {t("settings.codexRepair.enabled", {
                defaultValue: "Codex Desktop 健康检测",
              })}
            </p>
            <p className="text-xs text-muted-foreground">
              {t("settings.codexRepair.enabledDescription", {
                defaultValue: "启动 CC Switch 时检查 Codex Desktop 修复状态",
              })}
            </p>
          </div>
        </div>
        <Switch
          checked={enabled}
          onCheckedChange={onEnabledChange}
          aria-label={t("settings.codexRepair.enabled", {
            defaultValue: "Codex Desktop 健康检测",
          })}
        />
      </div>

      {enabled && (
        <div className="space-y-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-2 text-sm">
              {isChecking ? (
                <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
              ) : isHealthy ? (
                <CheckCircle2 className="h-4 w-4 text-emerald-500" />
              ) : (
                <AlertTriangle className="h-4 w-4 text-amber-500" />
              )}
              <span className="font-medium">
                {status ? stateLabels[status.state] : stateLabels.unknown}
              </span>
              {status?.packageVersion && (
                <span className="text-muted-foreground">
                  {status.packageVersion}
                </span>
              )}
            </div>
            <div className="flex items-center gap-2">
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => void checkStatus()}
                disabled={isChecking || repairRunning}
              >
                <RefreshCw
                  className={`mr-2 h-4 w-4 ${isChecking ? "animate-spin" : ""}`}
                />
                {t("settings.codexRepair.checkNow", {
                  defaultValue: "立即检测",
                })}
              </Button>
              <Button
                type="button"
                size="sm"
                variant={isHealthy ? "outline" : "default"}
                onClick={() => setConfirmOpen(true)}
                disabled={!canRepair || isLaunching || repairRunning}
              >
                {repairRunning ? (
                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                ) : (
                  <Wrench className="mr-2 h-4 w-4" />
                )}
                {repairRunning
                  ? t("settings.codexRepair.repairRunning", {
                      defaultValue: "修复进行中",
                    })
                  : t("settings.codexRepair.repair", {
                      defaultValue: "管理员修复",
                    })}
              </Button>
            </div>
          </div>

          {status && !status.runtimeInstalled && (
            <p className="text-xs text-amber-600 dark:text-amber-400">
              {t("settings.codexRepair.runtimeMissing", {
                defaultValue: "修复时将下载并校验 Fast Patch runtime",
              })}
            </p>
          )}

          {repairRunning && (
            <p className="text-xs text-sky-600 dark:text-sky-400">
              {t("settings.codexRepair.repairRunningDescription", {
                defaultValue:
                  "独立管理员窗口正在执行修复，请在该窗口查看进度。",
              })}
            </p>
          )}

          {status?.lastRepairError && !repairRunning && (
            <p className="break-words text-xs text-destructive">
              {status.lastRepairError}
            </p>
          )}

          {status && status.warnings.length > 0 && (
            <ul className="space-y-1 text-xs text-muted-foreground">
              {status.warnings.map((warning) => (
                <li key={warning} className="break-words">
                  {warning}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent role="alertdialog" className="max-w-md" zIndex="alert">
          <DialogHeader>
            <DialogTitle>
              {t("settings.codexRepair.confirmTitle", {
                defaultValue: "启动 Codex Desktop 修复？",
              })}
            </DialogTitle>
            <DialogDescription>
              {t("settings.codexRepair.confirmDescription", {
                defaultValue:
                  "将先备份 Codex 配置，再请求管理员权限运行可见的独立修复窗口。Codex Desktop 可能会关闭并重新启动。",
              })}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setConfirmOpen(false)}
              disabled={isLaunching}
            >
              {t("common.cancel")}
            </Button>
            <Button
              type="button"
              onClick={() => void launchRepair()}
              disabled={isLaunching}
            >
              {isLaunching ? (
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              ) : (
                <Wrench className="mr-2 h-4 w-4" />
              )}
              {t("settings.codexRepair.confirm", {
                defaultValue: "确认修复",
              })}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}
