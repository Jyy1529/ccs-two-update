import { useEffect, useId, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { isProxyAppId } from "@/config/appConfig";
import {
  modelValidationApi,
  type ValidationMode,
  type ValidationPlan,
  type ValidationProbe,
  type ValidationProtocol,
  type ValidationRun,
} from "@/lib/api/modelValidation";
import type { AppId } from "@/lib/api/types";
import type { Provider } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import {
  ModelValidationResults,
  ValidationTargetDetails,
} from "./ModelValidationResults";
import { ValidationModelPicker } from "./ValidationModelPicker";

const BASIC_PROBES: ValidationProbe[] = [
  "call",
  "stream",
  "tools",
  "structured",
  "image",
];
const ADVANCED_PROBES: ValidationProbe[] = [
  "output_limit",
  "cache",
  "thinking",
  "signature",
  "cross_signature",
];
const PROTOCOLS: ValidationProtocol[] = [
  "openai_chat",
  "openai_responses",
  "anthropic",
  "gemini",
];
const selectClass =
  "h-9 w-full rounded-md border border-input bg-background px-3 text-sm";

interface ModelValidationDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: AppId;
  provider: Provider;
  providers: Record<string, Provider>;
}

export function ModelValidationDialog({
  open,
  onOpenChange,
  appId,
  provider,
  providers,
}: ModelValidationDialogProps) {
  const { t } = useTranslation();
  const id = useId();
  const queryClient = useQueryClient();
  const [model, setModel] = useState("");
  const [protocol, setProtocol] = useState<ValidationProtocol | "auto">("auto");
  const [mode, setMode] = useState<ValidationMode>("direct");
  const [probes, setProbes] = useState<ValidationProbe[]>([...BASIC_PROBES]);
  const [comparisonProviderId, setComparisonProviderId] = useState("");
  const [comparisonModel, setComparisonModel] = useState("");
  const [comparisonProtocol, setComparisonProtocol] = useState<
    ValidationProtocol | "auto"
  >("auto");
  const [repeatCount, setRepeatCount] = useState(3);
  const [plan, setPlan] = useState<ValidationPlan | null>(null);
  const [expired, setExpired] = useState(false);
  const [runId, setRunId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const generation = useRef(0);
  const runRef = useRef<ValidationRun | null>(null);
  const cancelFailureMessage = useRef(t("modelValidation.cancelFailed"));
  cancelFailureMessage.current = t("modelValidation.cancelFailed");
  const historyKey = ["modelValidations", appId, provider.id];
  const history = useQuery({
    queryKey: historyKey,
    queryFn: () => modelValidationApi.list(appId, provider.id),
    enabled: open,
    retry: false,
  });
  const runQuery = useQuery({
    queryKey: ["modelValidationRun", runId],
    queryFn: () => modelValidationApi.get(runId!),
    enabled: open && Boolean(runId),
    retry: false,
    refetchInterval: (query) =>
      !busy && query.state.data?.status === "running" ? 1_000 : false,
  });
  const run = runQuery.data;
  const running = run?.status === "running";
  const needsComparison =
    probes.includes("cross_signature") || probes.includes("comparison");

  useEffect(() => {
    if (!run) return;
    runRef.current = run;
    if (run.status !== "running")
      void queryClient.invalidateQueries({
        queryKey: ["modelValidations", appId, provider.id],
      });
  }, [run, queryClient, appId, provider.id]);

  useEffect(() => {
    if (!plan) {
      setExpired(false);
      return;
    }
    const remaining = Date.parse(plan.expiresAt) - Date.now();
    if (!Number.isFinite(remaining) || remaining <= 0) {
      setExpired(true);
      return;
    }
    setExpired(false);
    const timer = window.setTimeout(() => setExpired(true), remaining);
    return () => window.clearTimeout(timer);
  }, [plan]);

  useEffect(() => {
    generation.current += 1;
    return () => {
      generation.current += 1;
      const activeRun = runRef.current;
      if (activeRun?.status === "running") {
        // Navigation/unmount also releases the finite, paid diagnostic job.
        void modelValidationApi
          .cancel(activeRun.id)
          .then(() => {
            void queryClient.invalidateQueries({
              queryKey: ["modelValidations", appId, provider.id],
            });
          })
          .catch(() => toast.error(cancelFailureMessage.current));
      }
    };
  }, [appId, provider.id, queryClient]);

  const resetPlan = () => {
    generation.current += 1;
    setPlan(null);
    setError("");
  };
  const toggleProbe = (probe: ValidationProbe, checked: boolean) => {
    resetPlan();
    setProbes((current) =>
      checked
        ? [...current.filter((item) => item !== probe), probe]
        : current.filter((item) => item !== probe),
    );
  };

  const prepare = async () => {
    if (busy || running) return;
    if (
      !model.trim() ||
      probes.length === 0 ||
      (needsComparison && (!comparisonProviderId || !comparisonModel.trim())) ||
      (probes.includes("comparison") &&
        (!Number.isInteger(repeatCount) || repeatCount < 2 || repeatCount > 5))
    ) {
      setError(t("modelValidation.invalidSelection"));
      return;
    }
    if (
      probes.includes("cross_signature") &&
      comparisonProviderId === provider.id
    ) {
      setError(t("modelValidation.differentTarget"));
      return;
    }
    setBusy(true);
    setError("");
    setPlan(null);
    const requestGeneration = ++generation.current;
    try {
      const prepared = await modelValidationApi.prepare({
        target: {
          appId,
          providerId: provider.id,
          model: model.trim(),
          ...(protocol !== "auto" ? { protocol } : {}),
        },
        mode,
        probes,
        ...(needsComparison
          ? {
              comparisonTarget: {
                appId,
                providerId: comparisonProviderId,
                model: comparisonModel.trim(),
                ...(comparisonProtocol !== "auto"
                  ? { protocol: comparisonProtocol }
                  : {}),
              },
            }
          : {}),
        ...(probes.includes("comparison") ? { repeatCount } : {}),
      });
      if (requestGeneration === generation.current) setPlan(prepared);
    } catch (cause) {
      if (requestGeneration === generation.current)
        setError(extractErrorMessage(cause));
    } finally {
      if (requestGeneration === generation.current) setBusy(false);
    }
  };

  const start = async () => {
    if (!plan || busy || expired || running) return;
    const expiresAt = Date.parse(plan.expiresAt);
    if (!Number.isFinite(expiresAt) || expiresAt <= Date.now()) {
      setExpired(true);
      return;
    }
    setBusy(true);
    setError("");
    const requestGeneration = generation.current;
    try {
      const started = await modelValidationApi.start(plan.id);
      if (requestGeneration !== generation.current) {
        if (started.status === "running") {
          try {
            await modelValidationApi.cancel(started.id);
          } catch {
            toast.error(cancelFailureMessage.current);
          }
        }
        return;
      }
      runRef.current = started;
      queryClient.setQueryData(["modelValidationRun", started.id], started);
      setRunId(started.id);
      setPlan(null);
      void queryClient.invalidateQueries({ queryKey: historyKey });
    } catch (cause) {
      if (requestGeneration === generation.current) {
        setError(extractErrorMessage(cause));
        setPlan(null); // Starting again requires a fresh target/config preview.
      }
    } finally {
      if (requestGeneration === generation.current) setBusy(false);
    }
  };

  const cancel = async (): Promise<boolean> => {
    const active = runRef.current;
    if (!active || active.status !== "running") return true;
    setBusy(true);
    setError("");
    try {
      // Discard any in-flight poll so its older running snapshot cannot replace
      // the terminal state returned after cancellation.
      await queryClient.cancelQueries({
        queryKey: ["modelValidationRun", active.id],
      });
      await modelValidationApi.cancel(active.id);
      const updated = await modelValidationApi.get(active.id);
      runRef.current = updated;
      queryClient.setQueryData(["modelValidationRun", updated.id], updated);
      void queryClient.invalidateQueries({ queryKey: historyKey });
      if (updated.status === "running") {
        setError(t("modelValidation.cancelling"));
        return false;
      }
      return true;
    } catch (cause) {
      setError(
        `${t("modelValidation.cancelFailed")} ${extractErrorMessage(cause)}`,
      );
      return false;
    } finally {
      setBusy(false);
    }
  };
  const close = async () => {
    if (busy) return;
    if (await cancel()) onOpenChange(false);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) void close();
      }}
    >
      <DialogContent
        className="max-w-3xl"
        onEscapeKeyDown={(event) => {
          if (busy) event.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>{t("modelValidation.title")}</DialogTitle>
          <DialogDescription>
            {t("modelValidation.description")}
          </DialogDescription>
        </DialogHeader>
        <div className="max-h-[70vh] space-y-5 overflow-y-auto px-6 py-4">
          <div className="rounded-lg border border-amber-500/30 bg-amber-500/10 p-3 text-xs leading-relaxed">
            <p>{t("modelValidation.safetyHint")}</p>
            <p className="mt-1">{t("modelValidation.oauthUnsupported")}</p>
          </div>
          <fieldset disabled={busy || running} className="space-y-4">
            <legend className="mb-2 text-sm font-medium">
              {provider.name} · {provider.id}
            </legend>
            <p className="text-xs text-muted-foreground">
              {t("modelValidation.fixedTarget")}
            </p>
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="space-y-1">
                <Label htmlFor={`${id}-model`}>
                  {t("modelValidation.model")}
                </Label>
                <ValidationModelPicker
                  id={`${id}-model`}
                  appId={appId}
                  provider={provider}
                  protocol={protocol}
                  enabled={open}
                  disabled={Boolean(busy || running)}
                  value={model}
                  onChange={(value) => {
                    resetPlan();
                    setModel(value);
                  }}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor={`${id}-protocol`}>
                  {t("modelValidation.protocol")}
                </Label>
                <select
                  id={`${id}-protocol`}
                  className={selectClass}
                  value={protocol}
                  onChange={(event) => {
                    resetPlan();
                    setProtocol(event.target.value as typeof protocol);
                  }}
                >
                  <option value="auto">
                    {t("modelValidation.autoProtocol")}
                  </option>
                  {PROTOCOLS.map((item) => (
                    <option key={item} value={item}>
                      {t(`modelValidation.protocols.${item}`)}
                    </option>
                  ))}
                </select>
              </div>
            </div>
            <div className="space-y-1">
              <Label htmlFor={`${id}-mode`}>{t("modelValidation.mode")}</Label>
              <select
                id={`${id}-mode`}
                className={selectClass}
                value={mode}
                onChange={(event) => {
                  resetPlan();
                  setMode(event.target.value as ValidationMode);
                }}
              >
                <option value="direct">
                  {t("modelValidation.modes.direct")}
                </option>
                <option value="ccs" disabled={!isProxyAppId(appId)}>
                  {t("modelValidation.modes.ccs")}
                </option>
              </select>
              <p className="text-xs text-muted-foreground">
                {t(
                  isProxyAppId(appId)
                    ? "modelValidation.modeHint"
                    : "modelValidation.ccsUnavailable",
                )}
              </p>
            </div>
            <ProbeChoices
              title={t("modelValidation.basic")}
              items={BASIC_PROBES}
              selected={probes}
              onChange={toggleProbe}
            />
            <details className="rounded-lg border border-border-default p-3">
              <summary className="cursor-pointer text-sm font-medium">
                {t("modelValidation.advanced")}
              </summary>
              <p className="my-2 text-xs text-muted-foreground">
                {t("modelValidation.advancedHint")}
              </p>
              <ProbeChoices
                items={ADVANCED_PROBES}
                selected={probes}
                onChange={toggleProbe}
              />
            </details>
            <div className="space-y-2 rounded-lg border border-border-default p-3">
              <label className="flex items-center gap-2 text-sm">
                <Checkbox
                  checked={probes.includes("comparison")}
                  onCheckedChange={(checked) =>
                    toggleProbe("comparison", checked)
                  }
                />
                {t("modelValidation.probes.comparison")}
              </label>
              <p className="text-xs text-muted-foreground">
                {t("modelValidation.comparisonHint")}
              </p>
              {probes.includes("comparison") && (
                <div className="space-y-1">
                  <Label htmlFor={`${id}-repeat`}>
                    {t("modelValidation.repeatCount")}
                  </Label>
                  <Input
                    id={`${id}-repeat`}
                    type="number"
                    min={2}
                    max={5}
                    value={repeatCount}
                    onChange={(event) => {
                      resetPlan();
                      setRepeatCount(Number(event.target.value));
                    }}
                  />
                </div>
              )}
            </div>
            {needsComparison && (
              <fieldset className="space-y-3 rounded-lg border border-amber-500/30 p-3">
                <legend className="px-1 text-sm font-medium">
                  {t("modelValidation.comparisonTarget")}
                </legend>
                <p className="text-xs text-muted-foreground">
                  {t("modelValidation.comparisonCost")}
                </p>
                <div className="space-y-1">
                  <Label htmlFor={`${id}-comparison-provider`}>
                    {t("modelValidation.provider")}
                  </Label>
                  <select
                    id={`${id}-comparison-provider`}
                    className={selectClass}
                    value={comparisonProviderId}
                    onChange={(event) => {
                      resetPlan();
                      setComparisonProviderId(event.target.value);
                    }}
                  >
                    <option value="">
                      {t("modelValidation.chooseTarget")}
                    </option>
                    {Object.values(providers).map((item) => (
                      <option
                        key={item.id}
                        value={item.id}
                        disabled={
                          probes.includes("cross_signature") &&
                          item.id === provider.id
                        }
                      >
                        {item.name} · {item.id}
                      </option>
                    ))}
                  </select>
                </div>
                <div className="space-y-1">
                  <Label htmlFor={`${id}-comparison-model`}>
                    {t("modelValidation.comparisonModel")}
                  </Label>
                  <ValidationModelPicker
                    id={`${id}-comparison-model`}
                    appId={appId}
                    provider={providers[comparisonProviderId]}
                    protocol={comparisonProtocol}
                    enabled={open}
                    disabled={Boolean(busy || running)}
                    value={comparisonModel}
                    onChange={(value) => {
                      resetPlan();
                      setComparisonModel(value);
                    }}
                  />
                </div>
                <div className="space-y-1">
                  <Label htmlFor={`${id}-comparison-protocol`}>
                    {t("modelValidation.comparisonProtocol")}
                  </Label>
                  <select
                    id={`${id}-comparison-protocol`}
                    className={selectClass}
                    value={comparisonProtocol}
                    onChange={(event) => {
                      resetPlan();
                      setComparisonProtocol(
                        event.target.value as typeof comparisonProtocol,
                      );
                    }}
                  >
                    <option value="auto">
                      {t("modelValidation.autoProtocol")}
                    </option>
                    {PROTOCOLS.map((item) => (
                      <option key={item} value={item}>
                        {t(`modelValidation.protocols.${item}`)}
                      </option>
                    ))}
                  </select>
                </div>
              </fieldset>
            )}
          </fieldset>
          {error && (
            <p role="alert" className="break-words text-sm text-destructive">
              {error}
            </p>
          )}
          {plan && (
            <section
              className="space-y-3 rounded-lg border border-blue-500/30 bg-blue-500/5 p-3"
              aria-label={t("modelValidation.prepared")}
            >
              <h4 className="text-sm font-medium">
                {t("modelValidation.prepared")}
              </h4>
              <ValidationTargetDetails target={plan.target} />
              {plan.comparisonTarget && (
                <div className="space-y-2 border-t pt-2">
                  <h5 className="text-sm font-medium">
                    {t("modelValidation.comparisonTarget")}
                  </h5>
                  <ValidationTargetDetails target={plan.comparisonTarget} />
                </div>
              )}
              <p className="text-xs">
                {plan.probes
                  .map((probe) => t(`modelValidation.probes.${probe}`))
                  .join(" · ")}
              </p>
              <dl className="grid grid-cols-2 gap-2 text-xs">
                <dt>{t("modelValidation.maxRequests")}</dt>
                <dd>{plan.maxRequests}</dd>
                <dt>{t("modelValidation.maxOutput")}</dt>
                <dd>{plan.maxOutputTokens}</dd>
                <dt>{t("modelValidation.maxDuration")}</dt>
                <dd>{plan.maxDurationSeconds} s</dd>
                <dt>{t("modelValidation.estimatedCost")}</dt>
                <dd>
                  {plan.estimatedCostUsd === null
                    ? t("modelValidation.costUnknown")
                    : `$${plan.estimatedCostUsd}`}
                </dd>
                <dt>{t("modelValidation.expiresAt")}</dt>
                <dd className="break-all">{plan.expiresAt}</dd>
              </dl>
              <p className="text-xs text-muted-foreground">
                {t("modelValidation.costDisclaimer")}
              </p>
              {plan.warnings.map((warning, index) => (
                <p
                  key={index}
                  className="break-words text-xs text-amber-700 dark:text-amber-300"
                >
                  {warning}
                </p>
              ))}
              {expired && (
                <p role="alert" className="text-sm">
                  {t("modelValidation.expired")}
                </p>
              )}
            </section>
          )}
          {runQuery.isError && (
            <div role="alert" className="space-y-2 text-sm">
              <p>{t("modelValidation.pollFailed")}</p>
              <Button
                size="sm"
                variant="outline"
                onClick={() => void runQuery.refetch()}
              >
                {t("common.retry")}
              </Button>
            </div>
          )}
          {run && <ModelValidationResults run={run} />}
          <section
            className="space-y-2 border-t pt-3"
            aria-label={t("modelValidation.history")}
          >
            <div className="flex items-center justify-between">
              <h4 className="text-sm font-medium">
                {t("modelValidation.history")}
              </h4>
              <Button
                size="sm"
                variant="ghost"
                disabled={history.isFetching}
                onClick={() => void history.refetch()}
              >
                {t("modelValidation.refresh")}
              </Button>
            </div>
            {history.isError && (
              <p role="alert" className="text-xs text-destructive">
                {t("modelValidation.historyFailed")}
              </p>
            )}
            {history.data?.length === 0 && (
              <p className="text-xs text-muted-foreground">
                {t("modelValidation.noHistory")}
              </p>
            )}
            {history.data?.map((item) => (
              <Button
                key={item.id}
                variant="outline"
                className="h-auto w-full flex-wrap justify-between gap-2 text-left text-xs"
                disabled={busy || Boolean(running && runId !== item.id)}
                onClick={() => {
                  setPlan(null);
                  runRef.current = item;
                  queryClient.setQueryData(
                    ["modelValidationRun", item.id],
                    item,
                  );
                  setRunId(item.id);
                }}
              >
                <span className="break-all">
                  {item.startedAt} · {item.plan.target.model}
                </span>
                <span>{t(`modelValidation.runStates.${item.status}`)}</span>
              </Button>
            ))}
          </section>
        </div>
        <DialogFooter>
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => void close()}
          >
            {t(running ? "modelValidation.cancelAndClose" : "common.close")}
          </Button>
          {running ? (
            <Button
              variant="destructive"
              disabled={busy}
              onClick={() => void cancel()}
            >
              {t("modelValidation.cancelRun")}
            </Button>
          ) : (
            <>
              <Button
                variant="outline"
                disabled={busy}
                onClick={() => void prepare()}
              >
                {busy && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                {t("modelValidation.prepare")}
              </Button>
              <Button
                disabled={!plan || busy || expired}
                onClick={() => void start()}
              >
                {t("modelValidation.start")}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function ProbeChoices({
  title,
  items,
  selected,
  onChange,
}: {
  title?: string;
  items: ValidationProbe[];
  selected: ValidationProbe[];
  onChange: (probe: ValidationProbe, checked: boolean) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="space-y-2">
      {title && <h4 className="text-sm font-medium">{title}</h4>}
      <div className="grid gap-2 sm:grid-cols-2">
        {items.map((probe) => (
          <label key={probe} className="flex items-center gap-2 text-sm">
            <Checkbox
              checked={selected.includes(probe)}
              onCheckedChange={(checked) => onChange(probe, checked)}
            />
            {t(`modelValidation.probes.${probe}`)}
          </label>
        ))}
      </div>
    </div>
  );
}
