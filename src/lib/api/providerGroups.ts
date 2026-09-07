import { invoke } from "@tauri-apps/api/core";
import type {
  BalanceQueryByCredentialsRequest,
  BalanceQueryResult,
  BalanceQueryTemplate,
  KeyPoolStrategy,
  ProviderGroup,
  ProviderGroupStatus,
  UsageResult,
} from "@/types";
import type { AppId } from "./types";

export interface ProviderGroupPolicy {
  enabled: boolean;
  strategy: KeyPoolStrategy;
  maxRetries: number;
  cooldownMs: number;
}

export const providerGroupsApi = {
  list(app: AppId): Promise<ProviderGroup[]> {
    return invoke("list_provider_groups", { app });
  },

  create(group: ProviderGroup): Promise<ProviderGroup> {
    return invoke("create_provider_group", { group });
  },

  update(group: ProviderGroup): Promise<ProviderGroup> {
    return invoke("update_provider_group", { group });
  },

  delete(groupId: string): Promise<void> {
    return invoke("delete_provider_group", { groupId });
  },

  reorder(app: AppId, groupIds: string[]): Promise<void> {
    return invoke("reorder_provider_groups", { app, groupIds });
  },

  reorderMembers(groupId: string, providerIds: string[]): Promise<void> {
    return invoke("reorder_provider_group_members", { groupId, providerIds });
  },

  moveProvider(
    app: AppId,
    providerId: string,
    groupId: string | null,
  ): Promise<void> {
    return invoke("move_provider_to_group", { app, providerId, groupId });
  },

  setPolicy(
    groupId: string,
    policy: ProviderGroupPolicy,
  ): Promise<ProviderGroup> {
    return invoke("set_group_key_pool_policy", {
      groupId,
      enabled: policy.enabled,
      strategy: policy.strategy,
      maxRetries: policy.maxRetries,
      cooldownMs: policy.cooldownMs,
    });
  },

  setMemberEnabled(
    app: AppId,
    providerId: string,
    enabled: boolean,
  ): Promise<void> {
    return invoke("set_provider_key_pool_enabled", {
      app,
      providerId,
      enabled,
    });
  },

  getStatus(groupId: string): Promise<ProviderGroupStatus> {
    return invoke("get_group_key_pool_status", { groupId });
  },

  getAutoGrouping(app: AppId): Promise<boolean> {
    return invoke("get_provider_auto_grouping", { app });
  },

  setAutoGrouping(app: AppId, enabled: boolean): Promise<ProviderGroup[]> {
    return invoke("set_provider_auto_grouping", { app, enabled });
  },

  listBalanceTemplates(): Promise<BalanceQueryTemplate[]> {
    return invoke("list_balance_query_templates");
  },

  setProviderBalanceTemplate(
    app: AppId,
    providerId: string,
    templateId: string | null,
  ): Promise<void> {
    return invoke("set_provider_balance_template", {
      app,
      providerId,
      templateId,
    });
  },

  saveBalanceTemplate(template: BalanceQueryTemplate): Promise<void> {
    return invoke("save_balance_query_template", { template });
  },

  deleteBalanceTemplate(templateId: string): Promise<boolean> {
    return invoke("delete_balance_query_template", { templateId });
  },

  queryProviderBalance(
    providerId: string,
    app: AppId,
  ): Promise<BalanceQueryResult> {
    return invoke("query_provider_balance", { providerId, app });
  },

  queryBalanceByCredentials(
    request: BalanceQueryByCredentialsRequest,
  ): Promise<UsageResult> {
    return invoke("query_balance_by_credentials", { request });
  },

  queryGroupBalances(groupId: string): Promise<BalanceQueryResult[]> {
    return invoke("query_group_balances", { groupId });
  },
};
