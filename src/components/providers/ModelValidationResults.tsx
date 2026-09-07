import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import type {
  ValidationRun,
  ValidationTargetSummary,
} from "@/lib/api/modelValidation";

// Endpoint previews must never expose URL credentials or query-string tokens.
export function validationEndpointLabel(endpoint: string): string {
  try {
    const url = new URL(endpoint);
    return `${url.protocol}//${url.host}${url.pathname}`;
  } catch {
    return "—";
  }
}

export function ValidationTargetDetails({
  target,
}: {
  target: ValidationTargetSummary;
}) {
  const { t } = useTranslation();
  return (
    <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 text-xs">
      <dt className="text-muted-foreground">{t("modelValidation.provider")}</dt>
      <dd className="break-all">
        {target.providerName} · {target.providerId}
      </dd>
      <dt className="text-muted-foreground">{t("modelValidation.endpoint")}</dt>
      <dd className="break-all font-mono">
        {validationEndpointLabel(target.endpoint)}
      </dd>
      <dt className="text-muted-foreground">
        {t("modelValidation.credential")}
      </dt>
      <dd>{target.credentialLabel}</dd>
      <dt className="text-muted-foreground">{t("modelValidation.model")}</dt>
      <dd className="break-all font-mono">{target.model}</dd>
      <dt className="text-muted-foreground">{t("modelValidation.protocol")}</dt>
      <dd>{t(`modelValidation.protocols.${target.protocol}`)}</dd>
    </dl>
  );
}

export function ModelValidationResults({ run }: { run: ValidationRun }) {
  const { t } = useTranslation();
  return (
    <section className="space-y-3" aria-label={t("modelValidation.results")}>
      <div className="space-y-2 rounded-lg border border-border-default p-3">
        <h4 className="text-sm font-medium">
          {t("modelValidation.runStatus", {
            status: t(`modelValidation.runStates.${run.status}`),
          })}
        </h4>
        <p className="text-xs text-muted-foreground">
          {run.startedAt} · {t(`modelValidation.modes.${run.plan.mode}`)}
        </p>
        <ValidationTargetDetails target={run.plan.target} />
        {run.plan.comparisonTarget && (
          <div className="space-y-2 border-t pt-2">
            <p className="text-xs font-medium">
              {t("modelValidation.comparisonTarget")}
            </p>
            <ValidationTargetDetails target={run.plan.comparisonTarget} />
          </div>
        )}
      </div>
      {run.results.length === 0 && (
        <p className="text-sm text-muted-foreground">
          {t(
            run.status === "running"
              ? "modelValidation.awaitingResults"
              : "modelValidation.noResults",
          )}
        </p>
      )}
      {run.results.map((result, index) => (
        <article
          key={`${result.probe}-${index}`}
          className="space-y-2 rounded-lg border border-border-default p-3"
          data-probe-status={result.status}
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h5 className="text-sm font-medium">
              {t(`modelValidation.probes.${result.probe}`)}
            </h5>
            <span
              className={cn(
                "rounded px-2 py-0.5 text-xs",
                result.status === "passed"
                  ? "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                  : result.status === "failed"
                    ? "bg-red-500/10 text-red-700 dark:text-red-300"
                    : "bg-muted text-muted-foreground",
              )}
            >
              {t(`modelValidation.resultStates.${result.status}`)}
            </span>
          </div>
          <p className="break-words text-sm">{result.summary}</p>
          <p className="text-xs text-muted-foreground">
            {t("modelValidation.resultMetrics", {
              count: result.requestCount,
              duration: result.durationMs,
            })}
          </p>
          {result.evidence.length > 0 && (
            <details className="text-xs">
              <summary className="cursor-pointer text-muted-foreground">
                {t("modelValidation.evidence")}
              </summary>
              <dl className="mt-2 space-y-2">
                {result.evidence.map((evidence, evidenceIndex) => (
                  <div key={evidenceIndex}>
                    <dt className="font-medium">{evidence.label}</dt>
                    <dd className="max-h-48 overflow-auto whitespace-pre-wrap break-all font-mono">
                      {evidence.value}
                    </dd>
                  </div>
                ))}
              </dl>
            </details>
          )}
        </article>
      ))}
      <p className="text-xs text-muted-foreground">
        {t("modelValidation.evidenceBoundary")}
      </p>
    </section>
  );
}
