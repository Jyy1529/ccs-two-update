import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { BalanceQueryTemplate, UsageResult } from "@/types";
import type { AppId } from "@/lib/api";
import { providerGroupsApi } from "@/lib/api/providerGroups";
import { providerGroupErrorMessage } from "@/utils/providerGroupErrors";
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
import { Textarea } from "@/components/ui/textarea";

interface BalanceQueryTemplateDialogProps {
  open: boolean;
  template?: BalanceQueryTemplate | null;
  onOpenChange: (open: boolean) => void;
  onSubmit: (template: BalanceQueryTemplate) => void;
  appId?: AppId;
  pending?: boolean;
}

const newId = () =>
  globalThis.crypto?.randomUUID?.() ?? `balance-template-${Date.now()}`;

function emptyTemplate(name: string): BalanceQueryTemplate {
  const now = Date.now();
  return {
    id: newId(),
    name,
    method: "GET",
    path: "/user/balance",
    query: {},
    headers: { Authorization: "Bearer {{apiKey}}" },
    body: null,
    remainingPath: "/balance",
    usedPath: null,
    totalPath: null,
    resetPath: null,
    errorPath: null,
    unit: "USD",
    currency: "USD",
    balanceScope: "unknown",
    timeoutSecs: 10,
    createdAt: now,
    updatedAt: now,
  };
}

export function BalanceQueryTemplateDialog({
  open,
  template,
  onOpenChange,
  onSubmit,
  appId = "codex",
  pending = false,
}: BalanceQueryTemplateDialogProps) {
  const { t } = useTranslation();
  const defaultName = t("providerGroups.templateDefaultName", {
    defaultValue: "Custom balance",
  });
  const [draft, setDraft] = useState<BalanceQueryTemplate>(
    template ?? emptyTemplate(defaultName),
  );
  const [headersText, setHeadersText] = useState(
    JSON.stringify(
      template?.headers ?? emptyTemplate(defaultName).headers,
      null,
      2,
    ),
  );
  const [queryText, setQueryText] = useState(
    JSON.stringify(template?.query ?? {}, null, 2),
  );
  const [error, setError] = useState("");
  const [testBaseUrl, setTestBaseUrl] = useState("");
  const [testKey, setTestKey] = useState("");
  const [testResult, setTestResult] = useState<UsageResult | null>(null);
  const [testPending, setTestPending] = useState(false);
  const testRequest = useRef(0);

  useEffect(() => {
    testRequest.current += 1;
    setTestKey("");
    setTestBaseUrl("");
    setTestResult(null);
    setTestPending(false);
    if (!open) return;
    const next = template ?? emptyTemplate(defaultName);
    setDraft(next);
    setHeadersText(JSON.stringify(next.headers, null, 2));
    setQueryText(JSON.stringify(next.query, null, 2));
    setError("");
    return () => {
      testRequest.current += 1;
    };
  }, [open, template?.id]);

  const update = <K extends keyof BalanceQueryTemplate>(
    key: K,
    value: BalanceQueryTemplate[K],
  ) => {
    setTestResult(null);
    setDraft((current) => ({ ...current, [key]: value }));
  };

  const parseDraft = (): BalanceQueryTemplate | null => {
    if (
      !draft.name.trim() ||
      !draft.path.trim() ||
      !draft.remainingPath.trim()
    ) {
      setError(
        t("providerGroups.templateRequired", {
          defaultValue:
            "Name, request path and remaining JSON path are required.",
        }),
      );
      return null;
    }
    try {
      const headers = JSON.parse(headersText) as Record<string, string>;
      const query = JSON.parse(queryText) as Record<string, string>;
      if (
        !headers ||
        Array.isArray(headers) ||
        typeof headers !== "object" ||
        Object.values(headers).some((value) => typeof value !== "string")
      )
        throw new Error(
          t("providerGroups.templateHeadersObject", {
            defaultValue: "Headers must be a JSON object",
          }),
        );
      if (
        !query ||
        Array.isArray(query) ||
        typeof query !== "object" ||
        Object.values(query).some((value) => typeof value !== "string")
      )
        throw new Error(
          t("providerGroups.templateQueryObject", {
            defaultValue: "Query must be a JSON object",
          }),
        );
      setError("");
      return {
        ...draft,
        name: draft.name.trim(),
        path: draft.path.trim(),
        remainingPath: draft.remainingPath.trim(),
        headers,
        query,
        timeoutSecs: Math.min(
          30,
          Math.max(1, Math.trunc(Number(draft.timeoutSecs)) || 10),
        ),
        updatedAt: Date.now(),
      };
    } catch (parseError) {
      setError(
        parseError instanceof Error && !(parseError instanceof SyntaxError)
          ? parseError.message
          : t("providerGroups.templateInvalidJson", {
              defaultValue: "Invalid JSON",
            }),
      );
      return null;
    }
  };

  const submit = () => {
    const template = parseDraft();
    if (template) onSubmit(template);
  };
  const testTemplate = async () => {
    const template = parseDraft();
    if (!template) return;
    if (!testBaseUrl.trim() || !testKey.trim()) {
      setError(
        t("providerGroups.testCredentialsRequired", {
          defaultValue: "Enter a Base URL and temporary API Key to test.",
        }),
      );
      return;
    }
    const request = ++testRequest.current;
    setTestPending(true);
    setTestResult(null);
    try {
      const result = await providerGroupsApi.queryBalanceByCredentials({
        appType: appId,
        baseUrl: testBaseUrl.trim(),
        apiKey: testKey.trim(),
        template,
      });
      if (request === testRequest.current) setTestResult(result);
    } catch (error) {
      if (request === testRequest.current)
        setError(providerGroupErrorMessage(error, t));
    } finally {
      if (request === testRequest.current) setTestPending(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent zIndex="top" className="max-h-[90vh] sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {template
              ? t("providerGroups.editBalanceTemplate", {
                  defaultValue: "Edit balance template",
                })
              : t("providerGroups.newBalanceTemplate", {
                  defaultValue: "New balance template",
                })}
          </DialogTitle>
          <DialogDescription>
            {t("providerGroups.templateDescription", {
              defaultValue:
                "Configure a GET/POST request. Use JSON Pointer paths such as",
            })}
            <code className="ml-1">/data/balance</code>.
          </DialogDescription>
        </DialogHeader>
        <div className="min-h-0 overflow-y-auto">
          <fieldset
            disabled={pending || testPending}
            className="grid min-w-0 gap-4 px-6 py-5 sm:grid-cols-2"
          >
            <div className="space-y-2 sm:col-span-2">
              <Label htmlFor="balance-template-name">
                {t("providerGroups.templateName", { defaultValue: "Name" })}
              </Label>
              <Input
                id="balance-template-name"
                value={draft.name}
                onChange={(e) => update("name", e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-method">
                {t("providerGroups.templateMethod", { defaultValue: "Method" })}
              </Label>
              <select
                id="balance-template-method"
                className="h-10 w-full rounded-md border border-input bg-background px-3 text-sm"
                value={draft.method}
                onChange={(e) =>
                  update("method", e.target.value as "GET" | "POST")
                }
              >
                <option value="GET">GET</option>
                <option value="POST">POST</option>
              </select>
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-timeout">
                {t("providerGroups.templateTimeout", {
                  defaultValue: "Timeout (seconds)",
                })}
              </Label>
              <Input
                id="balance-template-timeout"
                type="number"
                min={1}
                max={30}
                value={draft.timeoutSecs}
                onChange={(e) => update("timeoutSecs", Number(e.target.value))}
              />
            </div>
            <div className="space-y-2 sm:col-span-2">
              <Label htmlFor="balance-template-path">
                {t("providerGroups.templatePath", {
                  defaultValue: "Request path",
                })}
              </Label>
              <Input
                id="balance-template-path"
                value={draft.path}
                onChange={(e) => update("path", e.target.value)}
                placeholder="/v1/user/balance"
              />
              <p className="text-xs text-muted-foreground">
                {t("providerGroups.templatePathHint", {
                  defaultValue:
                    "Relative paths are appended to the Base URL. To query from the host root, enter a complete same-origin URL.",
                })}
              </p>
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-remaining">
                {t("providerGroups.templateRemaining", {
                  defaultValue: "Remaining JSON path",
                })}
              </Label>
              <Input
                id="balance-template-remaining"
                value={draft.remainingPath}
                onChange={(e) => update("remainingPath", e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-used">
                {t("providerGroups.templateUsed", {
                  defaultValue: "Used JSON path",
                })}
              </Label>
              <Input
                id="balance-template-used"
                value={draft.usedPath ?? ""}
                onChange={(e) => update("usedPath", e.target.value || null)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-total">
                {t("providerGroups.templateTotal", {
                  defaultValue: "Total JSON path",
                })}
              </Label>
              <Input
                id="balance-template-total"
                value={draft.totalPath ?? ""}
                onChange={(e) => update("totalPath", e.target.value || null)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-reset">
                {t("providerGroups.templateReset", {
                  defaultValue: "Reset JSON path",
                })}
              </Label>
              <Input
                id="balance-template-reset"
                value={draft.resetPath ?? ""}
                onChange={(e) => update("resetPath", e.target.value || null)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-unit">
                {t("providerGroups.templateUnit", { defaultValue: "Unit" })}
              </Label>
              <Input
                id="balance-template-unit"
                value={draft.unit ?? ""}
                onChange={(e) => update("unit", e.target.value || null)}
                placeholder="USD"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-error">
                {t("providerGroups.templateError", {
                  defaultValue: "Error JSON path",
                })}
              </Label>
              <Input
                id="balance-template-error"
                value={draft.errorPath ?? ""}
                onChange={(e) => update("errorPath", e.target.value || null)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-currency">
                {t("providerGroups.templateCurrency", {
                  defaultValue: "Currency (optional)",
                })}
              </Label>
              <Input
                id="balance-template-currency"
                value={draft.currency ?? ""}
                onChange={(e) =>
                  update(
                    "currency",
                    e.target.value.trim().toUpperCase() || null,
                  )
                }
                placeholder="USD / CNY"
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="balance-template-scope">
                {t("providerGroups.balanceScope", {
                  defaultValue: "Quota scope",
                })}
              </Label>
              <select
                id="balance-template-scope"
                value={draft.balanceScope ?? "unknown"}
                className="h-10 w-full rounded-md border border-input bg-background px-3 text-sm"
                onChange={(e) =>
                  update(
                    "balanceScope",
                    e.target.value as BalanceQueryTemplate["balanceScope"],
                  )
                }
              >
                <option value="unknown">
                  {t("providerGroups.scopeUnknown", {
                    defaultValue: "Unknown — do not sum",
                  })}
                </option>
                <option value="per_key">
                  {t("providerGroups.scopePerKey", {
                    defaultValue: "Independent quota per Key",
                  })}
                </option>
                <option value="account">
                  {t("providerGroups.scopeAccount", {
                    defaultValue: "Shared account balance — do not sum",
                  })}
                </option>
              </select>
            </div>
            <p className="text-xs text-muted-foreground sm:col-span-2">
              {t("providerGroups.scopeDescription", {
                defaultValue:
                  "Select independent quota only when the API reports a separate balance for each Key. Shared account balances must not be added repeatedly.",
              })}
            </p>
            <div className="space-y-2 sm:col-span-2">
              <Label htmlFor="balance-template-headers">
                {t("providerGroups.templateHeaders", {
                  defaultValue: "Headers JSON",
                })}
              </Label>
              <Textarea
                id="balance-template-headers"
                rows={4}
                value={headersText}
                onChange={(e) => {
                  setTestResult(null);
                  setHeadersText(e.target.value);
                }}
                className="font-mono text-xs"
              />
            </div>
            <div className="space-y-2 sm:col-span-2">
              <Label htmlFor="balance-template-query">
                {t("providerGroups.templateQuery", {
                  defaultValue: "Query JSON",
                })}
              </Label>
              <Textarea
                id="balance-template-query"
                rows={3}
                value={queryText}
                onChange={(e) => {
                  setTestResult(null);
                  setQueryText(e.target.value);
                }}
                className="font-mono text-xs"
              />
            </div>
            {draft.method === "POST" && (
              <div className="space-y-2 sm:col-span-2">
                <Label htmlFor="balance-template-body">
                  {t("providerGroups.templateBody", {
                    defaultValue: "Request body",
                  })}
                </Label>
                <Textarea
                  id="balance-template-body"
                  rows={4}
                  value={draft.body ?? ""}
                  onChange={(e) => update("body", e.target.value || null)}
                  className="font-mono text-xs"
                  placeholder='{"api_key":"{{apiKey}}"}'
                />
              </div>
            )}
            <div className="space-y-3 border-t border-border-default pt-4 sm:col-span-2">
              <h3 className="text-sm font-medium">
                {t("providerGroups.testTemplate", {
                  defaultValue: "Test template",
                })}
              </h3>
              <p className="text-xs text-muted-foreground">
                {t("providerGroups.testCredentialsNote", {
                  defaultValue:
                    "Temporary credentials are used only for this test and cleared when this dialog closes. Use placeholders in saved templates.",
                })}{" "}
                <code>{"{{apiKey}}"}</code>
              </p>
              <div className="space-y-2">
                <Label htmlFor="balance-test-base-url">Base URL</Label>
                <Input
                  id="balance-test-base-url"
                  value={testBaseUrl}
                  autoComplete="off"
                  onChange={(e) => {
                    setTestResult(null);
                    setTestBaseUrl(e.target.value);
                  }}
                  placeholder="https://api.example.com/v1"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="balance-test-key">
                  {t("providerGroups.temporaryApiKey", {
                    defaultValue: "Temporary API Key",
                  })}
                </Label>
                <Input
                  id="balance-test-key"
                  type="password"
                  autoComplete="off"
                  value={testKey}
                  onChange={(e) => {
                    setTestResult(null);
                    setTestKey(e.target.value);
                  }}
                />
              </div>
              <Button
                type="button"
                variant="outline"
                disabled={testPending || pending}
                onClick={() => void testTemplate()}
              >
                {testPending
                  ? t("providerGroups.testingTemplate", {
                      defaultValue: "Testing…",
                    })
                  : t("providerGroups.testTemplate", {
                      defaultValue: "Test template",
                    })}
              </Button>
              {testResult && (
                <div role="status" className="break-words text-xs">
                  {testResult.success
                    ? `${t("providerGroups.testSucceeded", { defaultValue: "Test succeeded" })}: ${(testResult.data ?? []).map((item) => `${item.remaining ?? "—"} ${item.unit ?? ""}`).join(", ")}`
                    : providerGroupErrorMessage(testResult.error, t)}
                </div>
              )}
            </div>
            {error && (
              <p
                className="text-sm text-destructive sm:col-span-2"
                role="alert"
              >
                {error}
              </p>
            )}
          </fieldset>
        </div>
        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={() => onOpenChange(false)}
          >
            {t("common.cancel", { defaultValue: "Cancel" })}
          </Button>
          <Button
            type="button"
            disabled={pending || testPending}
            onClick={submit}
          >
            {t("providerGroups.saveTemplate", {
              defaultValue: "Save template",
            })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
