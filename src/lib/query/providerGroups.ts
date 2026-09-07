import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { AppId } from "@/lib/api";
import {
  providerGroupsApi,
  type ProviderGroupPolicy,
} from "@/lib/api/providerGroups";
import type { BalanceQueryTemplate, ProviderGroup } from "@/types";
import { providerGroupErrorMessage } from "@/utils/providerGroupErrors";

export const providerGroupKeys = {
  all: ["provider-groups"] as const,
  list: (app: AppId) => ["provider-groups", app] as const,
  autoGrouping: (app: AppId) => ["provider-auto-grouping", app] as const,
  status: (groupId: string) => ["provider-group-status", groupId] as const,
  balanceTemplates: ["balance-query-templates"] as const,
};

function invalidateGroupSurface(
  queryClient: ReturnType<typeof useQueryClient>,
  app: AppId,
) {
  void queryClient.invalidateQueries({ queryKey: providerGroupKeys.list(app) });
  void queryClient.invalidateQueries({ queryKey: ["providers", app] });
  void queryClient.invalidateQueries({ queryKey: ["provider-group-status"] });
}

export function useProviderGroupsQuery(app: AppId, enabled = true) {
  return useQuery({
    queryKey: providerGroupKeys.list(app),
    queryFn: () => providerGroupsApi.list(app),
    enabled: enabled && Boolean(app),
    staleTime: 30_000,
  });
}

export function useProviderAutoGroupingQuery(app: AppId, enabled = true) {
  return useQuery({
    queryKey: providerGroupKeys.autoGrouping(app),
    queryFn: () => providerGroupsApi.getAutoGrouping(app),
    enabled: enabled && Boolean(app),
    placeholderData: false,
    staleTime: 30_000,
  });
}

export function useProviderGroupStatusQuery(
  groupId: string | undefined,
  enabled = true,
) {
  return useQuery({
    queryKey: providerGroupKeys.status(groupId ?? ""),
    queryFn: () => providerGroupsApi.getStatus(groupId!),
    enabled: enabled && Boolean(groupId),
    refetchInterval: 5_000,
  });
}

export function useBalanceQueryTemplatesQuery(enabled = true) {
  return useQuery({
    queryKey: providerGroupKeys.balanceTemplates,
    queryFn: () => providerGroupsApi.listBalanceTemplates(),
    enabled,
    staleTime: 60_000,
  });
}

export function useSaveBalanceQueryTemplateMutation() {
  const queryClient = useQueryClient();
  const { t } = useTranslation();
  return useMutation({
    mutationFn: (template: BalanceQueryTemplate) =>
      providerGroupsApi.saveBalanceTemplate(template),
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: providerGroupKeys.balanceTemplates,
      });
      toast.success(
        t("providerGroups.balanceTemplateSaved", {
          defaultValue: "Balance template saved",
        }),
      );
    },
    onError: (error) => toast.error(providerGroupErrorMessage(error, t)),
  });
}

function useGroupMutationError() {
  const { t } = useTranslation();
  return (error: unknown) => providerGroupErrorMessage(error, t);
}

export function useCreateProviderGroupMutation(app: AppId) {
  const queryClient = useQueryClient();
  const { t } = useTranslation();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: (group: ProviderGroup) => providerGroupsApi.create(group),
    onSuccess: () => {
      invalidateGroupSurface(queryClient, app);
      toast.success(
        t("providerGroups.created", { defaultValue: "Folder created" }),
      );
    },
    onError: (error) => toast.error(getError(error)),
  });
}

export function useUpdateProviderGroupMutation(
  app: AppId,
  { notifySuccess = true }: { notifySuccess?: boolean } = {},
) {
  const queryClient = useQueryClient();
  const { t } = useTranslation();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: (group: ProviderGroup) => providerGroupsApi.update(group),
    onSuccess: (group) => {
      invalidateGroupSurface(queryClient, app);
      void queryClient.invalidateQueries({
        queryKey: providerGroupKeys.status(group.id),
      });
      if (notifySuccess) {
        toast.success(
          t("providerGroups.updated", { defaultValue: "Folder updated" }),
        );
      }
    },
    onError: (error) => toast.error(getError(error)),
  });
}

export function useDeleteProviderGroupMutation(app: AppId) {
  const queryClient = useQueryClient();
  const { t } = useTranslation();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: (groupId: string) => providerGroupsApi.delete(groupId),
    onSuccess: (_, groupId) => {
      invalidateGroupSurface(queryClient, app);
      queryClient.removeQueries({
        queryKey: providerGroupKeys.status(groupId),
      });
      toast.success(
        t("providerGroups.deleted", { defaultValue: "Folder deleted" }),
      );
    },
    onError: (error) => toast.error(getError(error)),
  });
}

export function useMoveProviderToGroupMutation(app: AppId) {
  const queryClient = useQueryClient();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: ({
      providerId,
      groupId,
    }: {
      providerId: string;
      groupId: string | null;
    }) => providerGroupsApi.moveProvider(app, providerId, groupId),
    onSuccess: () => invalidateGroupSurface(queryClient, app),
    onError: (error) => toast.error(getError(error)),
  });
}

export function useReorderProviderGroupsMutation(app: AppId) {
  const queryClient = useQueryClient();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: (groupIds: string[]) =>
      providerGroupsApi.reorder(app, groupIds),
    onSuccess: () => invalidateGroupSurface(queryClient, app),
    onError: (error) => toast.error(getError(error)),
  });
}

export function useReorderProviderGroupMembersMutation(app: AppId) {
  const queryClient = useQueryClient();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: ({
      groupId,
      providerIds,
    }: {
      groupId: string;
      providerIds: string[];
    }) => providerGroupsApi.reorderMembers(groupId, providerIds),
    onSuccess: () => invalidateGroupSurface(queryClient, app),
    onError: (error) => toast.error(getError(error)),
  });
}

export function useSetProviderKeyPoolEnabledMutation(app: AppId) {
  const queryClient = useQueryClient();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: ({
      providerId,
      enabled,
    }: {
      providerId: string;
      enabled: boolean;
    }) => providerGroupsApi.setMemberEnabled(app, providerId, enabled),
    onSuccess: () => invalidateGroupSurface(queryClient, app),
    onError: (error) => toast.error(getError(error)),
  });
}

export function useSetProviderGroupPolicyMutation(app: AppId) {
  const queryClient = useQueryClient();
  const { t } = useTranslation();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: ({
      groupId,
      policy,
    }: {
      groupId: string;
      policy: ProviderGroupPolicy;
    }) => providerGroupsApi.setPolicy(groupId, policy),
    onSuccess: (group) => {
      invalidateGroupSurface(queryClient, app);
      void queryClient.invalidateQueries({
        queryKey: providerGroupKeys.status(group.id),
      });
      toast.success(
        t("providerGroups.policyUpdated", {
          defaultValue: "Key pool settings updated",
        }),
      );
    },
    onError: (error) => toast.error(getError(error)),
  });
}

export function useSetProviderAutoGroupingMutation(app: AppId) {
  const queryClient = useQueryClient();
  const getError = useGroupMutationError();
  return useMutation({
    mutationFn: (enabled: boolean) =>
      providerGroupsApi.setAutoGrouping(app, enabled),
    onSuccess: (groups, enabled) => {
      queryClient.setQueryData(providerGroupKeys.autoGrouping(app), enabled);
      queryClient.setQueryData(providerGroupKeys.list(app), groups);
      invalidateGroupSurface(queryClient, app);
    },
    onError: (error) => toast.error(getError(error)),
  });
}
