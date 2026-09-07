import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { KeyPoolStrategy, ProviderGroupStatus } from "@/types";
import type { ProviderGroupPolicy } from "@/lib/api/providerGroups";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { ArrowDown, ArrowUp } from "lucide-react";
import { isProxyAppId } from "@/config/appConfig";
import { providerGroupErrorMessage } from "@/utils/providerGroupErrors";

interface KeyPoolSettingsDialogProps {
  open: boolean;
  status: ProviderGroupStatus;
  onOpenChange: (open: boolean) => void;
  onSave: (policy: ProviderGroupPolicy) => void;
  onMemberEnabledChange: (providerId: string, enabled: boolean) => void;
  onMemberMove?: (providerId: string, offset: number) => void;
  pending?: boolean;
  error?: string;
}

export function KeyPoolSettingsDialog({
  open,
  status,
  onOpenChange,
  onSave,
  onMemberEnabledChange,
  onMemberMove,
  pending = false,
  error,
}: KeyPoolSettingsDialogProps) {
  const { t } = useTranslation();
  const { group } = status;
  const supported = isProxyAppId(group.appType);
  const [enabled, setEnabled] = useState(group.keyPoolEnabled);
  const [strategy, setStrategy] = useState<KeyPoolStrategy>(
    group.keyPoolStrategy,
  );
  const [maxRetries, setMaxRetries] = useState(group.keyPoolMaxRetries);
  const [cooldownMs, setCooldownMs] = useState(group.keyPoolCooldownMs);

  useEffect(() => {
    if (!open) return;
    setEnabled(group.keyPoolEnabled);
    setStrategy(group.keyPoolStrategy);
    setMaxRetries(group.keyPoolMaxRetries);
    setCooldownMs(group.keyPoolCooldownMs);
  }, [group.id, open]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent zIndex="top">
        <DialogHeader>
          <DialogTitle>
            {t("providerGroups.poolTitle", {
              group: group.name,
              defaultValue: "Key pool: {{group}}",
            })}
          </DialogTitle>
          <DialogDescription>
            {t("providerGroups.poolDescription", {
              defaultValue:
                "Only API-key providers with the same normalized Base URL can join.",
            })}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-5 overflow-y-auto px-6 py-5">
          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}
          {!supported && (
            <p role="note" className="text-sm text-amber-600">
              {t("providerGroups.poolUnsupported", {
                defaultValue:
                  "This app supports folders only; Key pools require a supported local proxy.",
              })}
            </p>
          )}
          {supported && status.proxyRunning === false && (
            <p role="note" className="text-xs text-muted-foreground">
              {t("providerGroups.proxyNotRunning", {
                defaultValue:
                  "The local proxy is not running. Saved pool settings apply when it starts.",
              })}
            </p>
          )}
          <div className="flex items-center justify-between gap-4">
            <Label htmlFor="key-pool-enabled">
              {t("providerGroups.poolEnabled", {
                defaultValue: "Enable key pool",
              })}
            </Label>
            <Switch
              id="key-pool-enabled"
              checked={enabled}
              disabled={pending || (!supported && !enabled)}
              onCheckedChange={setEnabled}
            />
          </div>

          <fieldset className="space-y-2">
            <legend className="text-sm font-medium">
              {t("providerGroups.strategy", { defaultValue: "Strategy" })}
            </legend>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="radio"
                name="key-pool-strategy"
                checked={strategy === "failover"}
                onChange={() => setStrategy("failover")}
              />
              {t("providerGroups.failover", { defaultValue: "Failover" })}
            </label>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="radio"
                name="key-pool-strategy"
                checked={strategy === "round_robin"}
                onChange={() => setStrategy("round_robin")}
              />
              {t("providerGroups.roundRobin", { defaultValue: "Round robin" })}
            </label>
          </fieldset>

          <div className="grid gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="key-pool-max-retries">
                {t("providerGroups.maxRetries", {
                  defaultValue: "Extra retries per Key",
                })}
              </Label>
              <Input
                id="key-pool-max-retries"
                type="number"
                min={0}
                max={100}
                value={maxRetries}
                onChange={(event) =>
                  setMaxRetries(
                    Math.min(
                      100,
                      Math.max(0, Math.trunc(Number(event.target.value)) || 0),
                    ),
                  )
                }
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="key-pool-cooldown">
                {t("providerGroups.cooldownMs", {
                  defaultValue: "Cooldown milliseconds",
                })}
              </Label>
              <Input
                id="key-pool-cooldown"
                type="number"
                min={0}
                max={60000}
                step={1000}
                value={cooldownMs}
                onChange={(event) =>
                  setCooldownMs(
                    Math.min(
                      60000,
                      Math.max(0, Math.trunc(Number(event.target.value)) || 0),
                    ),
                  )
                }
              />
            </div>
          </div>

          <p className="text-xs text-muted-foreground">
            {t("providerGroups.retryDescription", {
              defaultValue:
                "0 means no extra retries for a Key, then the next Key can be tried. An enabled Provider retry policy takes priority. Ordinary request errors do not switch Keys.",
            })}
          </p>

          <div className="space-y-2">
            <h3 className="text-sm font-medium">
              {t("providerGroups.members", { defaultValue: "Members" })}
            </h3>
            {status.members.map((member, index) => (
              <div
                key={member.providerId}
                className="flex items-center justify-between gap-4 border-t border-border-default py-2 first:border-t-0"
              >
                <div className="min-w-0">
                  <div className="truncate text-sm">{member.providerName}</div>
                  {(member.coolingDown ||
                    member.earlyProbe ||
                    member.error) && (
                    <div className="truncate text-xs text-muted-foreground">
                      {member.coolingDown
                        ? t("providerGroups.coolingRemaining", {
                            seconds: Math.ceil(
                              (member.cooldownRemainingMs ?? 0) / 1000,
                            ),
                            defaultValue: "Cooling down ({{seconds}}s)",
                          })
                        : member.earlyProbe
                          ? t("providerGroups.earlyProbe", {
                              defaultValue:
                                "All Keys were cooling; probing the earliest available Key",
                            })
                          : providerGroupErrorMessage(member.error, t)}
                    </div>
                  )}
                  {Boolean(member.consecutiveFailures) && (
                    <div className="text-xs text-muted-foreground">
                      {t("providerGroups.memberFailures", {
                        count: member.consecutiveFailures,
                        defaultValue: "Consecutive failures: {{count}}",
                      })}
                    </div>
                  )}
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  {onMemberMove && (
                    <>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7"
                        disabled={pending || index === 0}
                        onClick={() => onMemberMove(member.providerId, -1)}
                        aria-label={t("providerGroups.moveMemberUp", {
                          member: member.providerName,
                          defaultValue: "Move {{member}} up",
                        })}
                      >
                        <ArrowUp className="h-4 w-4" />
                      </Button>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7"
                        disabled={
                          pending || index === status.members.length - 1
                        }
                        onClick={() => onMemberMove(member.providerId, 1)}
                        aria-label={t("providerGroups.moveMemberDown", {
                          member: member.providerName,
                          defaultValue: "Move {{member}} down",
                        })}
                      >
                        <ArrowDown className="h-4 w-4" />
                      </Button>
                    </>
                  )}
                  <Switch
                    aria-label={member.providerName}
                    checked={member.keyPoolEnabled}
                    disabled={
                      pending ||
                      (!supported && !member.keyPoolEnabled) ||
                      (!member.keyPoolEnabled && Boolean(member.error))
                    }
                    onCheckedChange={(value) =>
                      onMemberEnabledChange(member.providerId, value)
                    }
                  />
                </div>
              </div>
            ))}
          </div>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t("common.cancel", { defaultValue: "Cancel" })}
          </Button>
          <Button
            disabled={
              pending ||
              (!supported && enabled) ||
              (enabled &&
                !status.members.some(
                  (member) => member.keyPoolEnabled && !member.error,
                ))
            }
            onClick={() =>
              onSave({ enabled, strategy, maxRetries, cooldownMs })
            }
          >
            {t("common.save", { defaultValue: "Save" })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
