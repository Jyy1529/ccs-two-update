import { useState } from "react";
import { ChevronDown, ChevronRight, RefreshCw } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Checkbox } from "@/components/ui/checkbox";
import { Collapsible, CollapsibleContent } from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import type { LocalProxyRetryErrorType, LocalProxyRetryPolicy } from "@/types";

export const DEFAULT_LOCAL_PROXY_RETRY_MESSAGE =
  "We're currently experiencing high demand, which may cause temporary errors.";

const ERROR_TYPES: LocalProxyRetryErrorType[] = [
  "rate_limit",
  "overloaded",
  "server_error",
  "network",
];

export type LocalProxyRetryPolicyValidationError =
  | "maxRetries"
  | "retryDelayMs"
  | "triggerRequired";

export function defaultLocalProxyRetryPolicy(): LocalProxyRetryPolicy {
  return {
    enabled: false,
    maxRetries: 0,
    retryDelayMs: 1000,
    customMessages: [DEFAULT_LOCAL_PROXY_RETRY_MESSAGE],
    errorTypes: [],
  };
}

export function normalizeLocalProxyRetryPolicy(
  policy?: LocalProxyRetryPolicy,
): LocalProxyRetryPolicy {
  const source = policy ?? defaultLocalProxyRetryPolicy();
  const seenMessages = new Set<string>();
  const messages = source.customMessages
    .map((message) => message.trim())
    .filter(Boolean)
    .filter((message) => {
      const key = message.toLowerCase();
      if (seenMessages.has(key)) return false;
      seenMessages.add(key);
      return true;
    });
  const errorTypes = ERROR_TYPES.filter((errorType) =>
    source.errorTypes.includes(errorType),
  );

  return {
    enabled: source.enabled ?? source.maxRetries > 0,
    maxRetries: Math.min(100, Math.max(0, Math.trunc(source.maxRetries))),
    retryDelayMs: Math.min(
      60_000,
      Math.max(1, Math.trunc(source.retryDelayMs)),
    ),
    customMessages: messages,
    errorTypes,
  };
}

export function validateLocalProxyRetryPolicy(
  policy: LocalProxyRetryPolicy,
): LocalProxyRetryPolicyValidationError | undefined {
  if (
    !Number.isInteger(policy.maxRetries) ||
    policy.maxRetries < 0 ||
    policy.maxRetries > 100
  ) {
    return "maxRetries";
  }
  if (
    !Number.isInteger(policy.retryDelayMs) ||
    policy.retryDelayMs < 1 ||
    policy.retryDelayMs > 60_000
  ) {
    return "retryDelayMs";
  }
  const normalized = normalizeLocalProxyRetryPolicy(policy);
  if (
    normalized.enabled &&
    normalized.customMessages.length === 0 &&
    normalized.errorTypes.length === 0
  ) {
    return "triggerRequired";
  }
  return undefined;
}

interface ProviderRetryPolicyConfigProps {
  value: LocalProxyRetryPolicy;
  onChange: (value: LocalProxyRetryPolicy) => void;
  idPrefix?: string;
}

export function ProviderRetryPolicyConfig({
  value,
  onChange,
  idPrefix = "provider-retry",
}: ProviderRetryPolicyConfigProps) {
  const { t } = useTranslation();
  const [isOpen, setIsOpen] = useState(false);
  const validationError = validateLocalProxyRetryPolicy(value);
  const enabled = value.enabled ?? value.maxRetries > 0;
  const title = t("providerAdvanced.retryPolicy", {
    defaultValue: "Local proxy automatic retry",
  });

  const errorTypeLabel: Record<LocalProxyRetryErrorType, string> = {
    rate_limit: t("providerAdvanced.retryErrorTypeRateLimit", {
      defaultValue: "Rate limit (HTTP 429)",
    }),
    overloaded: t("providerAdvanced.retryErrorTypeOverloaded", {
      defaultValue: "Overloaded (HTTP 503)",
    }),
    server_error: t("providerAdvanced.retryErrorTypeServerError", {
      defaultValue: "Other server errors (HTTP 5xx)",
    }),
    network: t("providerAdvanced.retryErrorTypeNetwork", {
      defaultValue: "Network and timeout errors",
    }),
  };

  return (
    <Collapsible
      open={isOpen}
      onOpenChange={setIsOpen}
      className="rounded-lg border border-border/50 bg-muted/20"
    >
      <div
        role="button"
        tabIndex={0}
        aria-label={title}
        aria-expanded={isOpen}
        className="flex w-full items-center justify-between gap-3 p-4 transition-colors hover:bg-muted/30"
        onClick={() => setIsOpen((open) => !open)}
        onKeyDown={(event) => {
          if (
            event.currentTarget === event.target &&
            (event.key === "Enter" || event.key === " ")
          ) {
            event.preventDefault();
            setIsOpen((open) => !open);
          }
        }}
      >
        <div className="flex min-w-0 items-center gap-3">
          <RefreshCw className="h-4 w-4 shrink-0 text-muted-foreground" />
          <span className="truncate font-medium">{title}</span>
        </div>
        <div className="flex shrink-0 items-center gap-3">
          <div
            className="flex items-center gap-2"
            onClick={(event) => event.stopPropagation()}
            onKeyDown={(event) => event.stopPropagation()}
          >
            <Label
              htmlFor={`${idPrefix}-enabled`}
              className="text-sm text-muted-foreground"
            >
              {t("providerAdvanced.retryProviderEnabled", {
                defaultValue: "Enable retries for this Provider",
              })}
            </Label>
            <Switch
              id={`${idPrefix}-enabled`}
              checked={enabled}
              onCheckedChange={(nextEnabled) => {
                onChange({ ...value, enabled: nextEnabled });
                if (nextEnabled) setIsOpen(true);
              }}
              aria-label={t("providerAdvanced.retryProviderEnabled", {
                defaultValue: "Enable retries for this Provider",
              })}
            />
          </div>
          {isOpen ? (
            <ChevronDown className="h-4 w-4 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-4 w-4 text-muted-foreground" />
          )}
        </div>
      </div>

      <CollapsibleContent>
        <div className="space-y-4 border-t border-border/50 p-4">
          <p className="text-sm text-muted-foreground">
            {t("providerAdvanced.retryPolicyDesc", {
              defaultValue:
                "Retry ordinary model requests on this provider before existing failover runs.",
            })}
          </p>
          <p className="text-xs text-muted-foreground">
            {t("providerAdvanced.retryGlobalHint", {
              defaultValue:
                "The global switch is in Settings → Proxy → Advanced proxy configuration.",
            })}
          </p>
          {value.maxRetries === 0 && enabled && (
            <p
              className="text-xs text-amber-600 dark:text-amber-400"
              role="status"
            >
              {t("providerAdvanced.retryZeroCountHint", {
                defaultValue:
                  "Unlimited retry is enabled. Matching errors keep retrying this Provider until success, a non-matching error, or request cancellation.",
              })}
            </p>
          )}

          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor={`${idPrefix}-max-retries`}>
                {t("providerAdvanced.retryMaxRetries", {
                  defaultValue: "Additional retries",
                })}
              </Label>
              <Input
                id={`${idPrefix}-max-retries`}
                type="number"
                min={0}
                max={100}
                step={1}
                value={value.maxRetries}
                onChange={(event) =>
                  onChange({ ...value, maxRetries: Number(event.target.value) })
                }
                aria-invalid={validationError === "maxRetries"}
              />
              <p className="text-xs text-muted-foreground">
                {t("providerAdvanced.retryMaxRetriesHint", {
                  defaultValue:
                    "When enabled, 0 means unlimited retries; finite values range from 1 to 100. Turn off the Provider switch to disable retries.",
                })}
              </p>
            </div>

            <div className="space-y-2">
              <Label htmlFor={`${idPrefix}-delay-ms`}>
                {t("providerAdvanced.retryDelayMs", {
                  defaultValue: "Retry interval (ms)",
                })}
              </Label>
              <Input
                id={`${idPrefix}-delay-ms`}
                type="number"
                min={1}
                max={60_000}
                step={1}
                value={value.retryDelayMs}
                onChange={(event) =>
                  onChange({
                    ...value,
                    retryDelayMs: Number(event.target.value),
                  })
                }
                aria-invalid={validationError === "retryDelayMs"}
              />
              <p className="text-xs text-muted-foreground">
                {t("providerAdvanced.retryDelayMsHint", {
                  defaultValue:
                    "Fixed delay between attempts, from 1 to 60000 ms.",
                })}
              </p>
            </div>
          </div>

          <fieldset className="space-y-2">
            <legend className="text-sm font-medium">
              {t("providerAdvanced.retryErrorTypes", {
                defaultValue: "Preset error types",
              })}
            </legend>
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
              {ERROR_TYPES.map((errorType) => {
                const id = `${idPrefix}-${errorType}`;
                return (
                  <div key={errorType} className="flex items-center gap-2">
                    <Checkbox
                      id={id}
                      checked={value.errorTypes.includes(errorType)}
                      onCheckedChange={(checked) =>
                        onChange({
                          ...value,
                          errorTypes: checked
                            ? [...value.errorTypes, errorType]
                            : value.errorTypes.filter(
                                (item) => item !== errorType,
                              ),
                        })
                      }
                    />
                    <Label htmlFor={id} className="font-normal">
                      {errorTypeLabel[errorType]}
                    </Label>
                  </div>
                );
              })}
            </div>
          </fieldset>

          <div className="space-y-2">
            <Label htmlFor={`${idPrefix}-messages`}>
              {t("providerAdvanced.retryCustomMessages", {
                defaultValue: "Error message contains",
              })}
            </Label>
            <Textarea
              id={`${idPrefix}-messages`}
              rows={4}
              value={value.customMessages.join("\n")}
              onChange={(event) =>
                onChange({
                  ...value,
                  customMessages: event.target.value.split(/\r?\n/),
                })
              }
              aria-invalid={validationError === "triggerRequired"}
            />
            <p className="text-xs text-muted-foreground">
              {t("providerAdvanced.retryCustomMessagesHint", {
                defaultValue:
                  "One case-insensitive substring per line. Only error packets are inspected.",
              })}
            </p>
          </div>

          {validationError && (
            <p className="text-sm text-destructive" role="alert">
              {t(`providerAdvanced.retryValidation.${validationError}`, {
                defaultValue:
                  validationError === "maxRetries"
                    ? "Additional retries must be an integer from 0 to 100."
                    : validationError === "retryDelayMs"
                      ? "Retry interval must be an integer from 1 to 60000 ms."
                      : "Choose at least one error type or enter an error message when retries are enabled.",
              })}
            </p>
          )}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}
