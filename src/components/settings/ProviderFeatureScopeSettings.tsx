import { useTranslation } from "react-i18next";
import { GitBranch, RefreshCw, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";
import { ProviderIcon } from "@/components/ProviderIcon";
import { APP_IDS } from "@/config/appConfig";
import {
  resolveProviderFeatureScopes,
  DEFAULT_PROVIDER_FEATURE_SCOPES,
} from "@/lib/featureScopes";
import type { SettingsFormState } from "@/hooks/useSettings";
import type { AppId } from "@/lib/api";
import type { FeatureScope, ProviderFeatureScopes } from "@/types";

interface ProviderFeatureScopeSettingsProps {
  settings: SettingsFormState;
  onChange: (updates: Partial<SettingsFormState>) => void;
}

const APP_ICON_NAME: Record<AppId, string> = {
  claude: "claude",
  "claude-desktop": "claude",
  codex: "openai",
  gemini: "gemini",
  grokbuild: "grok",
  opencode: "opencode",
  openclaw: "openclaw",
  hermes: "hermes",
  deepseek: "deepseek",
  pi: "pi",
};

const APP_NAME_KEY: Record<AppId, string> = {
  claude: "apps.claudeCode",
  "claude-desktop": "apps.claudeDesktop",
  codex: "apps.codex",
  gemini: "apps.gemini",
  grokbuild: "apps.grokbuild",
  opencode: "apps.opencode",
  openclaw: "apps.openclaw",
  hermes: "apps.hermes",
  deepseek: "apps.deepseek",
  pi: "apps.pi",
};

type FeatureKey = keyof ProviderFeatureScopes;

interface FeatureRowMeta {
  key: FeatureKey;
  icon: React.ReactNode;
  titleKey: string;
  descriptionKey: string;
  /** 可勾选的应用集合；角色路由与审批路由为 Codex 专属实现 */
  selectableApps: AppId[];
  /** 超出实现能力的说明（如「当前仅 Codex 支持」） */
  limitKey?: string;
}

const FEATURE_ROWS: FeatureRowMeta[] = [
  {
    key: "localProxyRetry",
    icon: <RefreshCw className="h-4 w-4 text-emerald-500" />,
    titleKey: "settings.featureScopes.localProxyRetry.title",
    descriptionKey: "settings.featureScopes.localProxyRetry.description",
    selectableApps: APP_IDS,
  },
  {
    key: "agentRoleRouting",
    icon: <GitBranch className="h-4 w-4 text-sky-500" />,
    titleKey: "settings.featureScopes.agentRoleRouting.title",
    descriptionKey: "settings.featureScopes.agentRoleRouting.description",
    selectableApps: ["codex"],
    limitKey: "settings.featureScopes.codexOnlyHint",
  },
  {
    key: "autoReviewRouting",
    icon: <ShieldCheck className="h-4 w-4 text-amber-500" />,
    titleKey: "settings.featureScopes.autoReviewRouting.title",
    descriptionKey: "settings.featureScopes.autoReviewRouting.description",
    selectableApps: ["codex"],
    limitKey: "settings.featureScopes.codexOnlyHint",
  },
];

export function ProviderFeatureScopeSettings({
  settings,
  onChange,
}: ProviderFeatureScopeSettingsProps) {
  const { t } = useTranslation();
  const scopes = resolveProviderFeatureScopes(settings.providerFeatureScopes);

  const updateScope = (key: FeatureKey, next: FeatureScope) => {
    onChange({
      providerFeatureScopes: {
        ...scopes,
        [key]: next,
      },
    });
  };

  const toggleApp = (key: FeatureKey, app: AppId) => {
    const scope = scopes[key];
    const apps = scope.apps.includes(app)
      ? scope.apps.filter((item) => item !== app)
      : [...scope.apps, app];
    updateScope(key, { ...scope, apps });
  };

  return (
    <section className="space-y-2">
      <header className="space-y-1">
        <h3 className="text-sm font-medium">
          {t("settings.featureScopes.title", {
            defaultValue: "Provider 功能生效范围",
          })}
        </h3>
        <p className="text-xs text-muted-foreground">
          {t("settings.featureScopes.description", {
            defaultValue:
              "控制以下功能启用与生效的应用；未勾选的应用不显示对应配置也不参与该功能。",
          })}
        </p>
      </header>
      <div className="space-y-2">
        {FEATURE_ROWS.map((row) => {
          const scope = scopes[row.key];
          const defaults = DEFAULT_PROVIDER_FEATURE_SCOPES[row.key];
          return (
            <div
              key={row.key}
              className="rounded-md border border-border-default bg-background p-3 space-y-2"
            >
              <div className="flex items-center justify-between gap-3">
                <div className="flex items-start gap-2.5">
                  <span className="mt-0.5">{row.icon}</span>
                  <div className="space-y-0.5">
                    <div className="text-sm font-medium">{t(row.titleKey)}</div>
                    <p className="text-xs text-muted-foreground">
                      {t(row.descriptionKey)}
                    </p>
                  </div>
                </div>
                <Switch
                  aria-label={t(row.titleKey, {
                    defaultValue: row.titleKey,
                  })}
                  checked={scope.enabled}
                  onCheckedChange={(enabled) =>
                    updateScope(row.key, {
                      ...scope,
                      enabled,
                      // 重新开启且应用列表为空时恢复默认应用，避免“开了却无处生效”
                      apps:
                        enabled && scope.apps.length === 0
                          ? defaults.apps
                          : scope.apps,
                    })
                  }
                />
              </div>
              <div className="flex flex-wrap items-center gap-1">
                {row.selectableApps.map((app) => {
                  const active = scope.apps.includes(app);
                  return (
                    <Button
                      key={app}
                      type="button"
                      size="sm"
                      disabled={!scope.enabled}
                      variant={active ? "default" : "ghost"}
                      onClick={() => toggleApp(row.key, app)}
                      className={cn(
                        "h-7 w-auto gap-1.5 px-2.5 text-xs",
                        active
                          ? "shadow-sm"
                          : "text-muted-foreground hover:text-foreground hover:bg-muted",
                      )}
                    >
                      <ProviderIcon
                        icon={APP_ICON_NAME[app]}
                        name={t(APP_NAME_KEY[app])}
                        size={13}
                      />
                      {t(APP_NAME_KEY[app])}
                    </Button>
                  );
                })}
                {row.limitKey && (
                  <span className="ml-1 text-[11px] text-muted-foreground">
                    {t(row.limitKey, {
                      defaultValue: "当前仅 Codex 支持",
                    })}
                  </span>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}
