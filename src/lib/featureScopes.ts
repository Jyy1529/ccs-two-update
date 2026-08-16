import { useSettingsQuery } from "@/lib/query";
import type { AppId } from "@/lib/api/types";
import type { FeatureScope, ProviderFeatureScopes } from "@/types";

/**
 * Provider 功能范围默认值（与后端 settings.rs 的 Default 保持一致）：
 * - 自动重试：Claude Code + Codex
 * - 子代理角色路由 / 审批模型路由：Codex 专属实现，仅 Codex
 */
export const DEFAULT_PROVIDER_FEATURE_SCOPES: ProviderFeatureScopes = {
  localProxyRetry: { enabled: true, apps: ["claude", "codex"] },
  agentRoleRouting: { enabled: true, apps: ["codex"] },
  autoReviewRouting: { enabled: true, apps: ["codex"] },
};

/** 补齐缺省字段，返回完整的功能范围配置 */
export function resolveProviderFeatureScopes(
  scopes?: ProviderFeatureScopes | null,
): ProviderFeatureScopes {
  if (!scopes) return DEFAULT_PROVIDER_FEATURE_SCOPES;
  return {
    localProxyRetry:
      scopes.localProxyRetry ?? DEFAULT_PROVIDER_FEATURE_SCOPES.localProxyRetry,
    agentRoleRouting:
      scopes.agentRoleRouting ??
      DEFAULT_PROVIDER_FEATURE_SCOPES.agentRoleRouting,
    autoReviewRouting:
      scopes.autoReviewRouting ??
      DEFAULT_PROVIDER_FEATURE_SCOPES.autoReviewRouting,
  };
}

/** 该功能对指定应用是否生效 */
export function featureScopeAllows(scope: FeatureScope, app: AppId): boolean {
  return scope.enabled && scope.apps.includes(app);
}

/** 从设置读取功能范围（加载中/未配置时按默认范围处理） */
export function useProviderFeatureScopes(): ProviderFeatureScopes {
  const { data } = useSettingsQuery();
  return resolveProviderFeatureScopes(data?.providerFeatureScopes);
}
