import { useEffect, useMemo, useRef, useState } from "react";
import { Bot, ChevronDown, ChevronRight, Loader2, Plus } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useProvidersQuery } from "@/lib/query";
import {
  fetchModelsForConfig,
  showFetchModelsError,
} from "@/lib/api/model-fetch";
import {
  extractCodexBaseUrl,
  extractCodexExperimentalBearerToken,
  extractCodexModelName,
} from "@/utils/providerConfigUtils";
import type {
  CodexAgentReasoningEffort,
  CodexAgentRoleRouting,
  CodexCatalogModel,
  Provider,
} from "@/types";

const CURRENT_PROVIDER_VALUE = "__current__";
const INHERIT_VALUE = "__inherit__";

const REASONING_EFFORTS: CodexAgentReasoningEffort[] = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
  "ultra",
];

export function defaultCodexAgentRoleRouting(): CodexAgentRoleRouting {
  return {
    enabled: false,
    frontend: {},
    backend: {},
  };
}

export function normalizeCodexAgentRoleRouting(
  value?: CodexAgentRoleRouting,
): CodexAgentRoleRouting {
  const clean = (input?: string) => input?.trim() || undefined;
  return {
    enabled: value?.enabled === true,
    frontend: {
      providerId: clean(value?.frontend?.providerId),
      upstreamModel: clean(value?.frontend?.upstreamModel),
      model: clean(value?.frontend?.model),
      reasoningEffort: value?.frontend?.reasoningEffort,
    },
    backend: {
      model: clean(value?.backend?.model),
      reasoningEffort: value?.backend?.reasoningEffort,
    },
  };
}

function catalogModels(provider?: Provider): string[] {
  const rawCatalog = provider?.settingsConfig?.modelCatalog as
    | { models?: unknown[] }
    | undefined;
  if (!Array.isArray(rawCatalog?.models)) return [];
  return rawCatalog.models
    .map((item) => {
      if (typeof item === "string") return item.trim();
      if (item && typeof item === "object" && "model" in item) {
        const model = (item as { model?: unknown }).model;
        return typeof model === "string" ? model.trim() : "";
      }
      return "";
    })
    .filter(Boolean);
}

function providerDefaultModel(provider?: Provider): string | undefined {
  const direct = provider?.settingsConfig?.model;
  if (typeof direct === "string" && direct.trim()) return direct.trim();
  const config = provider?.settingsConfig?.config;
  const configured = extractCodexModelName(
    typeof config === "string" ? config : undefined,
  );
  return configured?.trim() || catalogModels(provider)[0];
}

function providerApiKey(provider?: Provider): string {
  const auth = provider?.settingsConfig?.auth;
  if (auth && typeof auth === "object") {
    const apiKey = (auth as Record<string, unknown>).OPENAI_API_KEY;
    if (typeof apiKey === "string" && apiKey) return apiKey;
  }
  const config = provider?.settingsConfig?.config;
  return (
    extractCodexExperimentalBearerToken(
      typeof config === "string" ? config : "",
    ) || ""
  );
}

interface CodexAgentRoleRoutingConfigProps {
  value: CodexAgentRoleRouting;
  onChange: (value: CodexAgentRoleRouting) => void;
  ownerProviderId?: string;
  ownerDefaultModel?: string;
  ownerCatalogModels?: CodexCatalogModel[];
  ownerBaseUrl?: string;
  ownerApiKey?: string;
  ownerIsFullUrl?: boolean;
  ownerCustomUserAgent?: string;
  onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
  idPrefix?: string;
}

export function CodexAgentRoleRoutingConfig({
  value,
  onChange,
  ownerProviderId,
  ownerDefaultModel,
  ownerCatalogModels = [],
  ownerBaseUrl = "",
  ownerApiKey = "",
  ownerIsFullUrl = false,
  ownerCustomUserAgent,
  onRequestAddProvider,
  idPrefix = "codex-role",
}: CodexAgentRoleRoutingConfigProps) {
  const { t } = useTranslation();
  const { data } = useProvidersQuery("codex");
  const providers = data?.providers ?? {};
  const [isOpen, setIsOpen] = useState(false);
  const [isFetchingModels, setIsFetchingModels] = useState(false);
  const [fetchedModels, setFetchedModels] = useState<string[]>([]);
  const fetchModelsSeqRef = useRef(0);

  const frontend = value.frontend ?? {};
  const backend = value.backend ?? {};
  const selectedProvider = frontend.providerId
    ? providers[frontend.providerId]
    : ownerProviderId
      ? providers[ownerProviderId]
      : undefined;
  const selectedProviderMissing =
    Boolean(frontend.providerId) && !providers[frontend.providerId!];
  const alternativeProviders = Object.values(providers).filter(
    (provider) => provider.id !== ownerProviderId,
  );

  const selectedConfig = selectedProvider?.settingsConfig?.config;
  const fetchBaseUrl = frontend.providerId
    ? extractCodexBaseUrl(
        typeof selectedConfig === "string" ? selectedConfig : "",
      ) || ""
    : ownerBaseUrl;
  const fetchApiKey = frontend.providerId
    ? providerApiKey(selectedProvider)
    : ownerApiKey;
  const fetchIsFullUrl = frontend.providerId
    ? selectedProvider?.meta?.isFullUrl === true
    : ownerIsFullUrl;
  const fetchCustomUserAgent = frontend.providerId
    ? selectedProvider?.meta?.customUserAgent
    : ownerCustomUserAgent;
  const modelFetchIdentity = [
    frontend.providerId ?? CURRENT_PROVIDER_VALUE,
    fetchBaseUrl,
    fetchApiKey,
    fetchIsFullUrl ? "full" : "base",
    fetchCustomUserAgent ?? "",
  ].join("\u0000");

  useEffect(() => {
    fetchModelsSeqRef.current += 1;
    setFetchedModels([]);
    setIsFetchingModels(false);
  }, [modelFetchIdentity]);

  const targetModelCandidates = useMemo(() => {
    const values = new Set<string>();
    const add = (model?: string) => {
      const normalized = model?.trim();
      if (normalized) values.add(normalized);
    };

    if (frontend.providerId) {
      add(providerDefaultModel(selectedProvider));
      catalogModels(selectedProvider).forEach(add);
    } else {
      add(ownerDefaultModel);
      ownerCatalogModels.forEach((item) => add(item.model));
    }
    fetchedModels.forEach(add);
    return Array.from(values);
  }, [
    fetchedModels,
    frontend.providerId,
    ownerCatalogModels,
    ownerDefaultModel,
    selectedProvider,
  ]);

  const capabilityModels = useMemo(() => {
    const values = new Set<string>();
    const add = (model?: string) => {
      const normalized = model?.trim();
      if (normalized) values.add(normalized);
    };
    add(ownerDefaultModel);
    ownerCatalogModels.forEach((item) => add(item.model));
    return Array.from(values);
  }, [ownerCatalogModels, ownerDefaultModel]);

  const capabilityModelNotInVisibleProviderData =
    Boolean(frontend.model?.trim()) &&
    !capabilityModels.includes(frontend.model!.trim());

  const updateFrontend = (
    patch: Partial<NonNullable<CodexAgentRoleRouting["frontend"]>>,
  ) => onChange({ ...value, frontend: { ...frontend, ...patch } });
  const updateBackend = (
    patch: Partial<NonNullable<CodexAgentRoleRouting["backend"]>>,
  ) => onChange({ ...value, backend: { ...backend, ...patch } });

  const handleFetchModels = async () => {
    if (!fetchBaseUrl || !fetchApiKey) {
      showFetchModelsError(null, t, {
        hasApiKey: Boolean(fetchApiKey),
        hasBaseUrl: Boolean(fetchBaseUrl),
      });
      return;
    }

    const requestSeq = ++fetchModelsSeqRef.current;
    setIsFetchingModels(true);
    try {
      const models = await fetchModelsForConfig(
        fetchBaseUrl,
        fetchApiKey,
        fetchIsFullUrl,
        undefined,
        fetchCustomUserAgent,
      );
      if (fetchModelsSeqRef.current === requestSeq) {
        setFetchedModels(models.map((model) => model.id));
      }
    } catch (error) {
      if (fetchModelsSeqRef.current === requestSeq) {
        showFetchModelsError(error, t, {
          hasApiKey: true,
          hasBaseUrl: true,
        });
      }
    } finally {
      if (fetchModelsSeqRef.current === requestSeq) {
        setIsFetchingModels(false);
      }
    }
  };

  const effortSelect = (
    role: "frontend" | "backend",
    effort?: CodexAgentReasoningEffort,
  ) => (
    <Select
      value={effort ?? INHERIT_VALUE}
      onValueChange={(next) => {
        const reasoningEffort =
          next === INHERIT_VALUE
            ? undefined
            : (next as CodexAgentReasoningEffort);
        if (role === "frontend") updateFrontend({ reasoningEffort });
        else updateBackend({ reasoningEffort });
      }}
    >
      <SelectTrigger
        id={`${idPrefix}-${role}-reasoning`}
        aria-label={t("providerAdvanced.agentRoleReasoning", {
          defaultValue: "Reasoning effort",
        })}
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value={INHERIT_VALUE}>
          {t("providerAdvanced.agentRoleFollowMain", {
            defaultValue: "Follow main agent",
          })}
        </SelectItem>
        {REASONING_EFFORTS.map((item) => (
          <SelectItem key={item} value={item}>
            {item}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );

  return (
    <div className="rounded-lg border border-border/50 bg-muted/20">
      <div
        role="button"
        tabIndex={0}
        aria-expanded={isOpen}
        className="flex w-full items-center justify-between p-4 transition-colors hover:bg-muted/30"
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
        <div className="flex items-center gap-3">
          <Bot className="h-4 w-4 text-muted-foreground" />
          <div className="text-left">
            <p className="font-medium">
              {t("providerAdvanced.agentRoleRouting", {
                defaultValue: "Frontend/backend subagent model routing",
              })}
            </p>
            <p className="text-xs text-muted-foreground">
              {t("providerAdvanced.agentRoleRoutingSummary", {
                defaultValue:
                  "Route the frontend role through an independent Codex provider while the backend follows the current provider.",
              })}
            </p>
          </div>
        </div>
        <div className="flex items-center gap-3">
          <div
            className="flex items-center gap-2"
            onClick={(event) => event.stopPropagation()}
          >
            <Label htmlFor={`${idPrefix}-enabled`} className="text-sm">
              {t("providerAdvanced.agentRoleEnabled", {
                defaultValue: "Enable agent role routing",
              })}
            </Label>
            <Switch
              id={`${idPrefix}-enabled`}
              checked={value.enabled === true}
              onCheckedChange={(enabled) => {
                onChange({ ...value, enabled });
                if (enabled) setIsOpen(true);
              }}
            />
          </div>
          {isOpen ? (
            <ChevronDown className="h-4 w-4 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-4 w-4 text-muted-foreground" />
          )}
        </div>
      </div>

      {isOpen && (
        <div>
          <div className="space-y-5 border-t border-border/50 p-4">
            <p className="text-sm text-muted-foreground">
              {t("providerAdvanced.agentRoleRoutingDesc", {
                defaultValue:
                  "The frontend route requires Codex local proxy takeover. Provider B uses its own retry policy, then falls back to the owner provider A without changing the global current provider.",
              })}
            </p>

            <section className="space-y-4 rounded-md border border-border/50 p-4">
              <div>
                <p className="font-medium">
                  {t("providerAdvanced.agentRoleFrontend", {
                    defaultValue: "Frontend subagent",
                  })}
                </p>
                <p className="text-xs text-muted-foreground">
                  {t("providerAdvanced.agentRoleFrontendHint", {
                    defaultValue:
                      "Choose an independent provider and upstream model. Codex capability metadata follows the main agent unless explicitly overridden.",
                  })}
                </p>
              </div>

              <div className="space-y-2">
                <Label htmlFor={`${idPrefix}-frontend-provider`}>
                  {t("providerAdvanced.agentRoleFrontendProvider", {
                    defaultValue: "Frontend provider",
                  })}
                </Label>
                <div className="flex gap-2">
                  <Select
                    value={frontend.providerId ?? CURRENT_PROVIDER_VALUE}
                    onValueChange={(providerId) =>
                      updateFrontend({
                        providerId:
                          providerId === CURRENT_PROVIDER_VALUE
                            ? undefined
                            : providerId,
                      })
                    }
                  >
                    <SelectTrigger
                      id={`${idPrefix}-frontend-provider`}
                      aria-label={t(
                        "providerAdvanced.agentRoleFrontendProvider",
                        { defaultValue: "Frontend provider" },
                      )}
                      className="flex-1"
                    >
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={CURRENT_PROVIDER_VALUE}>
                        {t("providerAdvanced.agentRoleFollowCurrentProvider", {
                          defaultValue: "Follow current provider",
                        })}
                      </SelectItem>
                      {selectedProviderMissing && frontend.providerId && (
                        <SelectItem value={frontend.providerId}>
                          {t("providerAdvanced.agentRoleUnavailableProvider", {
                            defaultValue: "Unavailable provider",
                          })}{" "}
                          {`(${frontend.providerId})`}
                        </SelectItem>
                      )}
                      {alternativeProviders.map((provider) => (
                        <SelectItem key={provider.id} value={provider.id}>
                          {provider.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  {onRequestAddProvider && (
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() =>
                        onRequestAddProvider((providerId) =>
                          updateFrontend({ providerId }),
                        )
                      }
                    >
                      <Plus className="mr-2 h-4 w-4" />
                      {t("providerAdvanced.agentRoleAddProvider", {
                        defaultValue: "Add provider",
                      })}
                    </Button>
                  )}
                </div>
                {selectedProviderMissing && (
                  <p className="text-sm text-destructive" role="alert">
                    {t("providerAdvanced.agentRoleUnavailableProviderHint", {
                      defaultValue:
                        "The configured provider is unavailable. Runtime routing will fall back to the current provider.",
                    })}
                  </p>
                )}
              </div>

              <div className="space-y-2">
                <Label htmlFor={`${idPrefix}-frontend-upstream-model`}>
                  {t("providerAdvanced.agentRoleUpstreamModel", {
                    defaultValue: "Frontend upstream model",
                  })}
                </Label>
                <div className="flex gap-2">
                  <Input
                    id={`${idPrefix}-frontend-upstream-model`}
                    list={`${idPrefix}-upstream-models`}
                    value={frontend.upstreamModel ?? ""}
                    onChange={(event) =>
                      updateFrontend({ upstreamModel: event.target.value })
                    }
                    placeholder={t(
                      "providerAdvanced.agentRoleUpstreamModelPlaceholder",
                      { defaultValue: "Follow target provider default model" },
                    )}
                  />
                  <Button
                    type="button"
                    variant="outline"
                    disabled={isFetchingModels}
                    onClick={() => void handleFetchModels()}
                  >
                    {isFetchingModels && (
                      <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    )}
                    {t("providerAdvanced.agentRoleFetchModels", {
                      defaultValue: "Fetch models",
                    })}
                  </Button>
                </div>
                <datalist id={`${idPrefix}-upstream-models`}>
                  {targetModelCandidates.map((model) => (
                    <option key={model} value={model} />
                  ))}
                </datalist>
                <p className="text-xs text-muted-foreground">
                  {t("providerAdvanced.agentRoleUpstreamModelHint", {
                    defaultValue:
                      "This is the actual model sent to the selected provider. Free-form model IDs are supported.",
                  })}
                </p>
              </div>

              <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                <div className="space-y-2">
                  <Label htmlFor={`${idPrefix}-frontend-capability-model`}>
                    {t("providerAdvanced.agentRoleCapabilityModel", {
                      defaultValue: "Codex capability model",
                    })}
                  </Label>
                  <Input
                    id={`${idPrefix}-frontend-capability-model`}
                    list={`${idPrefix}-capability-models`}
                    value={frontend.model ?? ""}
                    onChange={(event) =>
                      updateFrontend({ model: event.target.value })
                    }
                    placeholder={t(
                      "providerAdvanced.agentRoleFollowMainAgent",
                      {
                        defaultValue: "Follow main agent",
                      },
                    )}
                  />
                  <datalist id={`${idPrefix}-capability-models`}>
                    {capabilityModels.map((model) => (
                      <option key={model} value={model} />
                    ))}
                  </datalist>
                  {capabilityModelNotInVisibleProviderData && (
                    <p className="text-sm text-muted-foreground">
                      {t(
                        "providerAdvanced.agentRoleCapabilityModelCatalogUnverified",
                        {
                          defaultValue:
                            "This model is not listed in the provider data shown here. The active shared catalog may include additional user-managed models.",
                        },
                      )}
                    </p>
                  )}
                </div>
                <div className="space-y-2">
                  <Label htmlFor={`${idPrefix}-frontend-reasoning`}>
                    {t("providerAdvanced.agentRoleReasoning", {
                      defaultValue: "Reasoning effort",
                    })}
                  </Label>
                  {effortSelect("frontend", frontend.reasoningEffort)}
                </div>
              </div>
            </section>

            <section className="space-y-4 rounded-md border border-border/50 p-4">
              <div>
                <p className="font-medium">
                  {t("providerAdvanced.agentRoleBackend", {
                    defaultValue: "Backend subagent",
                  })}
                </p>
                <p className="text-xs text-muted-foreground">
                  {t("providerAdvanced.agentRoleBackendProviderHint", {
                    provider: ownerProviderId
                      ? providers[ownerProviderId]?.name || ownerProviderId
                      : t("providerAdvanced.agentRoleCurrentProvider", {
                          defaultValue: "current provider",
                        }),
                    defaultValue:
                      "The backend role always uses the owner provider: {{provider}}.",
                  })}
                </p>
              </div>
              <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                <div className="space-y-2">
                  <Label htmlFor={`${idPrefix}-backend-model`}>
                    {t("providerAdvanced.agentRoleBackendModel", {
                      defaultValue: "Backend capability model",
                    })}
                  </Label>
                  <Input
                    id={`${idPrefix}-backend-model`}
                    list={`${idPrefix}-capability-models`}
                    value={backend.model ?? ""}
                    onChange={(event) =>
                      updateBackend({ model: event.target.value })
                    }
                    placeholder={t(
                      "providerAdvanced.agentRoleFollowMainAgent",
                      {
                        defaultValue: "Follow main agent",
                      },
                    )}
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor={`${idPrefix}-backend-reasoning`}>
                    {t("providerAdvanced.agentRoleReasoning", {
                      defaultValue: "Reasoning effort",
                    })}
                  </Label>
                  {effortSelect("backend", backend.reasoningEffort)}
                </div>
              </div>
            </section>
          </div>
        </div>
      )}
    </div>
  );
}
