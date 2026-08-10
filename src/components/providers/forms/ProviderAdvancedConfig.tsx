import type { ReactNode } from "react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight, Coins } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type {
  CodexAgentRoleRouting,
  CodexCatalogModel,
  LocalProxyRetryPolicy,
} from "@/types";
import { ProviderRetryPolicyConfig } from "./ProviderRetryPolicyConfig";
import { CodexAgentRoleRoutingConfig } from "./CodexAgentRoleRoutingConfig";

export type PricingModelSourceOption = "inherit" | "request" | "response";

interface ProviderPricingConfig {
  enabled: boolean;
  costMultiplier?: string;
  pricingModelSource: PricingModelSourceOption;
}

interface ProviderAdvancedOptionsSectionProps {
  children: ReactNode;
  defaultOpen?: boolean;
}

export function ProviderAdvancedOptionsSection({
  children,
  defaultOpen = false,
}: ProviderAdvancedOptionsSectionProps) {
  const { t } = useTranslation();
  const [isOpen, setIsOpen] = useState(defaultOpen);

  return (
    <Collapsible
      open={isOpen}
      onOpenChange={setIsOpen}
      className="rounded-lg border border-border-default p-4"
    >
      <CollapsibleTrigger asChild>
        <Button
          type="button"
          variant={null}
          size="sm"
          className="h-8 w-full justify-start gap-1.5 px-0 text-sm font-medium text-foreground hover:opacity-70"
        >
          {isOpen ? (
            <ChevronDown className="h-4 w-4" />
          ) : (
            <ChevronRight className="h-4 w-4" />
          )}
          {t("providerForm.advancedOptionsToggle", {
            defaultValue: "Advanced Options",
          })}
        </Button>
      </CollapsibleTrigger>
      {!isOpen && (
        <p className="ml-1 mt-1 text-xs text-muted-foreground">
          {t("providerForm.advancedOptionsHint", {
            defaultValue:
              "Includes API format, auth field, model mapping, automatic retry, and pricing settings.",
          })}
        </p>
      )}
      <CollapsibleContent className="space-y-4 pt-3">
        {children}
      </CollapsibleContent>
    </Collapsible>
  );
}

interface ProviderAdvancedConfigProps {
  pricingConfig: ProviderPricingConfig;
  onPricingConfigChange: (config: ProviderPricingConfig) => void;
  retryPolicy?: LocalProxyRetryPolicy;
  onRetryPolicyChange?: (policy: LocalProxyRetryPolicy) => void;
  codexAgentRoleRouting?: CodexAgentRoleRouting;
  onCodexAgentRoleRoutingChange?: (routing: CodexAgentRoleRouting) => void;
  ownerProviderId?: string;
  ownerDefaultModel?: string;
  ownerCatalogModels?: CodexCatalogModel[];
  ownerBaseUrl?: string;
  ownerApiKey?: string;
  ownerIsFullUrl?: boolean;
  ownerCustomUserAgent?: string;
  onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
}

export function ProviderAdvancedConfig({
  pricingConfig,
  onPricingConfigChange,
  retryPolicy,
  onRetryPolicyChange,
  codexAgentRoleRouting,
  onCodexAgentRoleRoutingChange,
  ownerProviderId,
  ownerDefaultModel,
  ownerCatalogModels,
  ownerBaseUrl,
  ownerApiKey,
  ownerIsFullUrl,
  ownerCustomUserAgent,
  onRequestAddProvider,
}: ProviderAdvancedConfigProps) {
  const { t } = useTranslation();
  const [isPricingConfigOpen, setIsPricingConfigOpen] = useState(
    pricingConfig.enabled,
  );

  useEffect(() => {
    setIsPricingConfigOpen(pricingConfig.enabled);
  }, [pricingConfig.enabled]);

  return (
    <div className="space-y-4">
      {retryPolicy && onRetryPolicyChange && (
        <ProviderRetryPolicyConfig
          value={retryPolicy}
          onChange={onRetryPolicyChange}
        />
      )}
      {codexAgentRoleRouting && onCodexAgentRoleRoutingChange && (
        <CodexAgentRoleRoutingConfig
          value={codexAgentRoleRouting}
          onChange={onCodexAgentRoleRoutingChange}
          ownerProviderId={ownerProviderId}
          ownerDefaultModel={ownerDefaultModel}
          ownerCatalogModels={ownerCatalogModels}
          ownerBaseUrl={ownerBaseUrl}
          ownerApiKey={ownerApiKey}
          ownerIsFullUrl={ownerIsFullUrl}
          ownerCustomUserAgent={ownerCustomUserAgent}
          onRequestAddProvider={onRequestAddProvider}
        />
      )}
      <Collapsible
        open={isPricingConfigOpen}
        onOpenChange={setIsPricingConfigOpen}
        className="rounded-lg border border-border/50 bg-muted/20"
      >
        <div
          role="button"
          tabIndex={0}
          aria-expanded={isPricingConfigOpen}
          className="flex w-full items-center justify-between p-4 transition-colors hover:bg-muted/30"
          onClick={() => setIsPricingConfigOpen(!isPricingConfigOpen)}
          onKeyDown={(event) => {
            if (
              event.currentTarget === event.target &&
              (event.key === "Enter" || event.key === " ")
            ) {
              event.preventDefault();
              setIsPricingConfigOpen(!isPricingConfigOpen);
            }
          }}
        >
          <div className="flex items-center gap-3">
            <Coins className="h-4 w-4 text-muted-foreground" />
            <span className="font-medium">
              {t("providerAdvanced.pricingConfig", {
                defaultValue: "Pricing configuration",
              })}
            </span>
          </div>
          <div className="flex items-center gap-3">
            <div
              className="flex items-center gap-2"
              onClick={(event) => event.stopPropagation()}
              onKeyDown={(event) => event.stopPropagation()}
            >
              <Label
                htmlFor="pricing-config-enabled"
                className="text-sm text-muted-foreground"
              >
                {t("providerAdvanced.useCustomPricing", {
                  defaultValue: "Use separate configuration",
                })}
              </Label>
              <Switch
                id="pricing-config-enabled"
                checked={pricingConfig.enabled}
                onCheckedChange={(checked) => {
                  onPricingConfigChange({ ...pricingConfig, enabled: checked });
                  if (checked) setIsPricingConfigOpen(true);
                }}
              />
            </div>
            {isPricingConfigOpen ? (
              <ChevronDown className="h-4 w-4 text-muted-foreground" />
            ) : (
              <ChevronRight className="h-4 w-4 text-muted-foreground" />
            )}
          </div>
        </div>
        <CollapsibleContent>
          <div className="space-y-4 border-t border-border/50 p-4">
            <p className="text-sm text-muted-foreground">
              {t("providerAdvanced.pricingConfigDesc", {
                defaultValue:
                  "Configure separate pricing parameters for this Provider. Global defaults are used when disabled.",
              })}
            </p>
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              <div className="space-y-2">
                <Label htmlFor="cost-multiplier">
                  {t("providerAdvanced.costMultiplier", {
                    defaultValue: "Cost multiplier",
                  })}
                </Label>
                <Input
                  id="cost-multiplier"
                  type="number"
                  step="0.01"
                  min="0"
                  inputMode="decimal"
                  value={pricingConfig.costMultiplier || ""}
                  onChange={(event) =>
                    onPricingConfigChange({
                      ...pricingConfig,
                      costMultiplier: event.target.value || undefined,
                    })
                  }
                  placeholder={t("providerAdvanced.costMultiplierPlaceholder", {
                    defaultValue: "Leave empty to use the global default (1)",
                  })}
                  disabled={!pricingConfig.enabled}
                />
                <p className="text-xs text-muted-foreground">
                  {t("providerAdvanced.costMultiplierHint", {
                    defaultValue:
                      "Actual cost = base cost × multiplier. Decimals such as 1.5 are supported.",
                  })}
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="pricing-model-source">
                  {t("providerAdvanced.pricingModelSourceLabel", {
                    defaultValue: "Pricing model",
                  })}
                </Label>
                <Select
                  value={pricingConfig.pricingModelSource}
                  onValueChange={(value) =>
                    onPricingConfigChange({
                      ...pricingConfig,
                      pricingModelSource: value as PricingModelSourceOption,
                    })
                  }
                  disabled={!pricingConfig.enabled}
                >
                  <SelectTrigger id="pricing-model-source">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="inherit">
                      {t("providerAdvanced.pricingModelSourceInherit", {
                        defaultValue: "Inherit global default",
                      })}
                    </SelectItem>
                    <SelectItem value="request">
                      {t("providerAdvanced.pricingModelSourceRequest", {
                        defaultValue: "Request model",
                      })}
                    </SelectItem>
                    <SelectItem value="response">
                      {t("providerAdvanced.pricingModelSourceResponse", {
                        defaultValue: "Response model",
                      })}
                    </SelectItem>
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {t("providerAdvanced.pricingModelSourceHint", {
                    defaultValue:
                      "Choose whether pricing matches the request or response model.",
                  })}
                </p>
              </div>
            </div>
          </div>
        </CollapsibleContent>
      </Collapsible>
    </div>
  );
}
