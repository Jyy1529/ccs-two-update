import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { useDndMonitor } from "@dnd-kit/core";
import {
  SortableContext,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  FolderPlus,
  Loader2,
  Network,
  Pencil,
  Plus,
  Settings2,
} from "lucide-react";
import type { AppId } from "@/lib/api";
import type {
  BalanceQueryResult as BalanceResult,
  BalanceQueryTemplate,
  Provider,
  ProviderGroup,
} from "@/types";
import {
  useCreateProviderGroupMutation,
  useDeleteProviderGroupMutation,
  useMoveProviderToGroupMutation,
  useReorderProviderGroupsMutation,
  useReorderProviderGroupMembersMutation,
  useProviderGroupStatusQuery,
  useSetProviderGroupPolicyMutation,
  useSetProviderKeyPoolEnabledMutation,
  useBalanceQueryTemplatesQuery,
  useSaveBalanceQueryTemplateMutation,
  useUpdateProviderGroupMutation,
} from "@/lib/query/providerGroups";
import { providerGroupsApi } from "@/lib/api/providerGroups";
import { Button } from "@/components/ui/button";
import { KeyPoolSettingsDialog } from "@/components/providers/KeyPoolSettingsDialog";
import {
  ProviderGroupDialog,
  type ProviderGroupSubmit,
} from "@/components/providers/ProviderGroupDialog";
import { SortableProviderGroup } from "@/components/providers/ProviderGroupHeader";
import { ProviderFolderSelect } from "./ProviderFolderSelect";
import { Switch } from "@/components/ui/switch";
import { BalanceQueryTemplateDialog } from "@/components/providers/BalanceQueryTemplateDialog";
import {
  ProviderBalanceButton,
  ProviderBalanceResult,
} from "./ProviderBalanceResult";
import { getPoolBalanceTotal } from "@/utils/poolBalance";
import { providerGroupErrorMessage } from "@/utils/providerGroupErrors";
import { ProviderBalanceSettingsDialog } from "./ProviderBalanceSettingsDialog";

interface ProviderGroupSectionsProps {
  appId: AppId;
  toolbarContainer?: HTMLDivElement | null;
  groups: ProviderGroup[];
  providers: Provider[];
  allProviders: Provider[];
  autoGrouping: boolean;
  autoGroupingPending: boolean;
  onAutoGroupingChange: (enabled: boolean) => void;
  onToggleCollapsed: (group: ProviderGroup) => void;
  renderProvider: (
    provider: Provider,
    groupActions?: ReactNode,
    balanceSummary?: ReactNode,
  ) => ReactNode;
}

function blankGroup(
  appId: AppId,
  name: string,
  sortIndex: number,
): ProviderGroup {
  return {
    id: "",
    appType: appId,
    name,
    kind: "manual",
    normalizedBaseUrl: null,
    sortIndex,
    collapsed: false,
    keyPoolEnabled: false,
    keyPoolStrategy: "failover",
    keyPoolMaxRetries: 0,
    keyPoolCooldownMs: 1000,
    balanceTemplateId: null,
    createdAt: 0,
    updatedAt: 0,
  };
}

export function ProviderGroupSections({
  appId,
  toolbarContainer,
  groups,
  providers,
  allProviders,
  autoGrouping,
  autoGroupingPending,
  onAutoGroupingChange,
  onToggleCollapsed,
  renderProvider,
}: ProviderGroupSectionsProps) {
  const { t } = useTranslation();
  const [editingGroup, setEditingGroup] = useState<ProviderGroup | null>();
  const [poolGroup, setPoolGroup] = useState<ProviderGroup | null>(null);
  const [balanceGroup, setBalanceGroup] = useState<ProviderGroup | null>(null);
  const [balanceProviderId, setBalanceProviderId] = useState<string>();
  const balanceProvider = allProviders.find(
    (provider) => provider.id === balanceProviderId,
  );
  const [balances, setBalances] = useState<Record<string, BalanceResult>>({});
  const [balancePending, setBalancePending] = useState(false);
  const [balanceScope, setBalanceScope] = useState("");
  const balanceRequest = useRef(0);
  const currentApp = useRef(appId);
  const previousApp = useRef(appId);
  currentApp.current = appId;
  const [templateDialog, setTemplateDialog] = useState<
    BalanceQueryTemplate | null | undefined
  >();
  const [templateTargetGroupId, setTemplateTargetGroupId] = useState<string>();
  const createGroup = useCreateProviderGroupMutation(appId);
  const updateGroup = useUpdateProviderGroupMutation(appId);
  const deleteGroup = useDeleteProviderGroupMutation(appId);
  const moveProvider = useMoveProviderToGroupMutation(appId);
  const reorderGroups = useReorderProviderGroupsMutation(appId);
  const reorderMembers = useReorderProviderGroupMembersMutation(appId);
  const setPolicy = useSetProviderGroupPolicyMutation(appId);
  const setMemberEnabled = useSetProviderKeyPoolEnabledMutation(appId);
  const { data: balanceTemplates = [] } = useBalanceQueryTemplatesQuery();
  const saveBalanceTemplate = useSaveBalanceQueryTemplateMutation();
  const {
    data: poolStatus,
    error: poolError,
    isPending: poolLoading,
  } = useProviderGroupStatusQuery(poolGroup?.id, Boolean(poolGroup));

  useEffect(() => {
    if (previousApp.current !== appId) {
      previousApp.current = appId;
      balanceRequest.current += 1;
      setBalances({});
      setBalancePending(false);
      setBalanceGroup(null);
      setBalanceProviderId(undefined);
      setPoolGroup(null);
      setEditingGroup(undefined);
      setTemplateDialog(undefined);
      setTemplateTargetGroupId(undefined);
    }
    return () => {
      balanceRequest.current += 1;
    };
  }, [appId]);

  const sortedGroups = useMemo(
    () =>
      [...groups].sort(
        (a, b) => a.sortIndex - b.sortIndex || a.id.localeCompare(b.id),
      ),
    [groups],
  );
  const knownGroups = new Set(sortedGroups.map((group) => group.id));
  const groupedProviders = new Map<string, Provider[]>();
  const ungroupedProviders: Provider[] = [];
  for (const provider of providers) {
    const groupId = provider.meta?.providerGroupId;
    if (!groupId || !knownGroups.has(groupId))
      ungroupedProviders.push(provider);
    else {
      const members = groupedProviders.get(groupId) ?? [];
      members.push(provider);
      groupedProviders.set(groupId, members);
    }
  }
  for (const members of groupedProviders.values()) {
    members.sort(
      (a, b) =>
        (a.meta?.providerGroupSortIndex ?? Number.MAX_SAFE_INTEGER) -
          (b.meta?.providerGroupSortIndex ?? Number.MAX_SAFE_INTEGER) ||
        (a.sortIndex ?? Number.MAX_SAFE_INTEGER) -
          (b.sortIndex ?? Number.MAX_SAFE_INTEGER) ||
        a.id.localeCompare(b.id),
    );
  }

  const saveGroup = ({
    name,
    groupId,
    icon,
    iconColor,
  }: ProviderGroupSubmit) => {
    const existing = groups.find((group) => group.id === groupId);
    const onSuccess = () => setEditingGroup(undefined);
    if (existing)
      updateGroup.mutate({ ...existing, name, icon, iconColor }, { onSuccess });
    else
      createGroup.mutate(
        { ...blankGroup(appId, name, groups.length), icon, iconColor },
        { onSuccess },
      );
  };
  const moveGroup = (groupId: string, toIndex: number) => {
    const ids = sortedGroups.map((group) => group.id);
    const fromIndex = ids.indexOf(groupId);
    if (
      fromIndex < 0 ||
      toIndex < 0 ||
      toIndex >= ids.length ||
      fromIndex === toIndex ||
      reorderGroups.isPending
    )
      return;
    ids.splice(toIndex, 0, ids.splice(fromIndex, 1)[0]);
    reorderGroups.mutate(ids, {
      onSuccess: () =>
        toast.success(
          t("provider.sortUpdated", { defaultValue: "排序已更新" }),
        ),
    });
  };
  useDndMonitor({
    onDragEnd({ active, over }) {
      const source = active.data.current;
      const target = over?.data.current;
      if (
        source?.type === "provider-group" &&
        target?.type === "provider-group" &&
        source.appId === appId &&
        target.appId === appId
      ) {
        moveGroup(
          source.groupId,
          sortedGroups.findIndex((group) => group.id === target.groupId),
        );
      }
    },
  });
  const runBalanceQuery = async (
    scope: string,
    targets: Provider[],
    query: () => Promise<BalanceResult[]>,
  ) => {
    if (balancePending) return;
    const request = ++balanceRequest.current;
    setBalanceScope(scope);
    setBalancePending(true);
    setBalances((current) =>
      Object.fromEntries(
        Object.entries(current).filter(
          ([id]) => !targets.some((p) => p.id === id),
        ),
      ),
    );
    const isCurrent = () =>
      request === balanceRequest.current && currentApp.current === appId;
    try {
      const results = await query();
      if (isCurrent()) {
        setBalances((current) => ({
          ...current,
          ...Object.fromEntries(results.map((r) => [r.providerId, r])),
        }));
        const failure = results.find((result) => result.status !== "success");
        if (failure) {
          const message = providerGroupErrorMessage(failure.error, t);
          if (
            failure.error?.includes("[balance_builtin_unknown]") &&
            targets.length === 1
          ) {
            toast.error(message, {
              action: {
                label: t("providerGroups.balanceTemplate"),
                onClick: () => setBalanceProviderId(failure.providerId),
              },
            });
          } else toast.error(message);
        }
      }
    } catch (error) {
      if (isCurrent()) {
        toast.error(providerGroupErrorMessage(error, t));
        setBalances((current) => ({
          ...current,
          ...Object.fromEntries(
            targets.map((p) => [
              p.id,
              {
                providerId: p.id,
                providerName: p.name,
                status: "failed",
                data: [],
                error: providerGroupErrorMessage(error, t),
              },
            ]),
          ),
        }));
      }
    } finally {
      if (isCurrent()) setBalancePending(false);
    }
  };
  const queryBalances = (group: ProviderGroup) => {
    if (balancePending) return;
    if (group.collapsed) onToggleCollapsed(group);
    setBalanceGroup(group);
    void runBalanceQuery(
      `group:${group.id}`,
      allProviders.filter(
        (provider) => provider.meta?.providerGroupId === group.id,
      ),
      () => providerGroupsApi.queryGroupBalances(group.id),
    );
  };
  const clearGroupBalances = (groupIds: string[]) => {
    const memberIds = new Set(
      allProviders
        .filter(
          (provider) =>
            provider.meta?.providerGroupId &&
            groupIds.includes(provider.meta.providerGroupId),
        )
        .map((provider) => provider.id),
    );
    setBalances((current) =>
      Object.fromEntries(
        Object.entries(current).filter(([id]) => !memberIds.has(id)),
      ),
    );
  };
  const renderProviderCard = (provider: Provider, groupId?: string) =>
    renderProvider(
      provider,
      <>
        {sortedGroups.length > 0 && (
          <ProviderFolderSelect
            provider={provider}
            groups={sortedGroups}
            groupId={groupId}
            disabled={moveProvider.isPending}
            onChange={(nextGroupId) =>
              moveProvider.mutate({
                providerId: provider.id,
                groupId: nextGroupId,
              })
            }
          />
        )}
        <ProviderBalanceButton
          name={provider.name}
          disabled={balancePending}
          pending={balancePending && balanceScope === `provider:${provider.id}`}
          onQuery={() =>
            void runBalanceQuery(
              `provider:${provider.id}`,
              [provider],
              async () => [
                await providerGroupsApi.queryProviderBalance(
                  provider.id,
                  appId,
                ),
              ],
            )
          }
        />
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="h-8 w-8 shrink-0"
          disabled={balancePending}
          aria-label={t("providerGroups.balanceSettingsFor", {
            provider: provider.name,
          })}
          title={t("providerGroups.balanceSettingsFor", {
            provider: provider.name,
          })}
          onClick={() => setBalanceProviderId(provider.id)}
        >
          <Settings2 className="h-3.5 w-3.5" />
        </Button>
      </>,
      <ProviderBalanceResult result={balances[provider.id]} />,
    );

  const renderTemplateControls = (group: ProviderGroup) => (
    <div className="flex min-w-0 items-center gap-1">
      <label
        htmlFor={`balance-template-${group.id}`}
        className="hidden text-xs text-muted-foreground lg:inline"
      >
        {t("providerGroups.balanceTemplate", {
          defaultValue: "Balance template",
        })}
      </label>
      <select
        id={`balance-template-${group.id}`}
        disabled={balancePending || updateGroup.isPending}
        aria-label={t("providerGroups.balanceTemplateFor", {
          group: group.name,
          defaultValue: "Balance template for {{group}}",
        })}
        className="h-7 max-w-36 rounded-md border border-border-default bg-background px-2 text-xs"
        value={group.balanceTemplateId ?? ""}
        onChange={(event) =>
          updateGroup.mutate(
            { ...group, balanceTemplateId: event.target.value || null },
            { onSuccess: () => clearGroupBalances([group.id]) },
          )
        }
      >
        <option value="">
          {t("providerGroups.builtinDetection", {
            defaultValue: "Built-in detection",
          })}
        </option>
        {balanceTemplates.map((template) => (
          <option key={template.id} value={template.id}>
            {template.name}
          </option>
        ))}
      </select>
      <Button
        type="button"
        size="icon"
        variant="ghost"
        className="h-7 w-7 shrink-0"
        disabled={balancePending}
        aria-label={t("providerGroups.newBalanceTemplate", {
          defaultValue: "New balance template",
        })}
        title={t("providerGroups.newBalanceTemplate", {
          defaultValue: "New balance template",
        })}
        onClick={() => {
          setTemplateTargetGroupId(group.id);
          setTemplateDialog(null);
        }}
      >
        <Plus className="h-4 w-4" />
      </Button>
      {group.balanceTemplateId && (
        <Button
          type="button"
          size="icon"
          variant="ghost"
          className="h-7 w-7 shrink-0"
          disabled={balancePending}
          aria-label={t("providerGroups.editBalanceTemplate", {
            defaultValue: "Edit balance template",
          })}
          title={t("providerGroups.editBalanceTemplate", {
            defaultValue: "Edit balance template",
          })}
          onClick={() => {
            setTemplateTargetGroupId(group.id);
            setTemplateDialog(
              balanceTemplates.find(
                (template) => template.id === group.balanceTemplateId,
              ) ?? null,
            );
          }}
        >
          <Pencil className="h-4 w-4" />
        </Button>
      )}
    </div>
  );

  return (
    <div className="space-y-3">
      {toolbarContainer &&
        createPortal(
          <div className="flex shrink-0 items-center gap-1.5">
            <div
              className="flex items-center gap-1 px-1.5 h-8 rounded-lg bg-muted/50 transition-all"
              title={t("providerGroups.autoGroupingDescription", {
                defaultValue: "按相同 Base URL 自动整理当前应用的供应商",
              })}
            >
              {autoGroupingPending ? (
                <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
              ) : (
                <Network
                  className={
                    autoGrouping
                      ? "h-4 w-4 transition-colors text-emerald-500 status-heartbeat"
                      : "h-4 w-4 transition-colors text-muted-foreground"
                  }
                />
              )}
              <Switch
                aria-label={t("providerGroups.autoGrouping", {
                  defaultValue: "按 Base URL 自动分组",
                })}
                checked={autoGrouping}
                disabled={autoGroupingPending}
                onCheckedChange={onAutoGroupingChange}
              />
            </div>
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="h-8 w-8 rounded-lg bg-muted/50"
              aria-label={t("providerGroups.newFolder", {
                defaultValue: "新建文件夹",
              })}
              title={t("providerGroups.newFolder", {
                defaultValue: "新建文件夹",
              })}
              onClick={() => setEditingGroup(null)}
            >
              <FolderPlus className="h-4 w-4" />
            </Button>
          </div>,
          toolbarContainer,
        )}

      <SortableContext
        items={sortedGroups.map((group) => `provider-group:${group.id}`)}
        strategy={verticalListSortingStrategy}
      >
        {sortedGroups.map((group, groupIndex) => {
          const members = groupedProviders.get(group.id) ?? [];
          const allMembers = allProviders.filter(
            (provider) => provider.meta?.providerGroupId === group.id,
          );
          const memberResults = allMembers.flatMap((p) =>
            balances[p.id] ? [balances[p.id]] : [],
          );
          const total = group.keyPoolEnabled
            ? getPoolBalanceTotal(
                memberResults,
                allMembers
                  .filter((p) => p.meta?.keyPoolEnabled)
                  .map((p) => p.id),
              )
            : null;
          return (
            <SortableProviderGroup
              key={group.id}
              group={group}
              memberCount={allMembers.length}
              dragDisabled={reorderGroups.isPending}
              onMoveUp={
                groupIndex > 0 && !reorderGroups.isPending
                  ? () => moveGroup(group.id, groupIndex - 1)
                  : undefined
              }
              onMoveDown={
                groupIndex < sortedGroups.length - 1 && !reorderGroups.isPending
                  ? () => moveGroup(group.id, groupIndex + 1)
                  : undefined
              }
              onToggleCollapsed={() => onToggleCollapsed(group)}
              onRename={() => setEditingGroup(group)}
              onDelete={() => deleteGroup.mutate(group.id)}
              onConfigurePool={() => setPoolGroup(group)}
              onQueryBalances={() => void queryBalances(group)}
              isQueryingBalances={balancePending}
              balanceTemplateControls={renderTemplateControls(group)}
            >
              {!group.collapsed && (
                <div className="space-y-3 p-3">
                  {balancePending && balanceScope === `group:${group.id}` && (
                    <p
                      role="status"
                      className="flex items-center gap-2 text-xs text-muted-foreground"
                    >
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                      {t("common.loading", { defaultValue: "Loading..." })}
                    </p>
                  )}
                  <SortableContext
                    items={members.map((provider) => provider.id)}
                    strategy={verticalListSortingStrategy}
                  >
                    {members.map((provider) => (
                      <div key={provider.id}>
                        {renderProviderCard(provider, group.id)}
                      </div>
                    ))}
                  </SortableContext>
                  {memberResults.length > 0 && group.keyPoolEnabled && (
                    <p
                      className="border-t border-border-default pt-3 text-xs text-muted-foreground"
                      aria-live="polite"
                    >
                      {total
                        ? t("providerGroups.poolTotal", {
                            amount: total.remaining.toLocaleString(undefined, {
                              maximumFractionDigits: 8,
                            }),
                            unit: total.unit,
                            defaultValue: "Pool balance: {{amount}} {{unit}}",
                          })
                        : t("providerGroups.poolTotalUnavailable", {
                            defaultValue:
                              "Shown per Key. A total requires successful, independent quotas with matching units and currencies for every enabled member.",
                          })}
                    </p>
                  )}
                  {balanceGroup?.id === group.id && allMembers.length === 0 && (
                    <p className="text-xs text-muted-foreground">
                      {t("providerGroups.emptyFolder", {
                        defaultValue: "This folder has no providers yet.",
                      })}
                    </p>
                  )}
                </div>
              )}
            </SortableProviderGroup>
          );
        })}
      </SortableContext>

      {ungroupedProviders.map((provider) => (
        <div key={provider.id}>{renderProviderCard(provider)}</div>
      ))}

      <ProviderGroupDialog
        open={editingGroup !== undefined}
        appId={appId}
        group={editingGroup ?? null}
        onOpenChange={(open) => !open && setEditingGroup(undefined)}
        onSubmit={saveGroup}
        pending={createGroup.isPending || updateGroup.isPending}
      />
      {balanceProvider && (
        <ProviderBalanceSettingsDialog
          key={`${appId}:${balanceProvider.id}`}
          appId={appId}
          provider={balanceProvider}
          group={groups.find(
            (group) => group.id === balanceProvider.meta?.providerGroupId,
          )}
          onOpenChange={(open) => !open && setBalanceProviderId(undefined)}
          onChanged={() => {
            if (currentApp.current === appId) setBalances({});
          }}
        />
      )}
      {poolGroup && (
        <KeyPoolSettingsDialog
          open
          status={
            poolStatus ?? {
              group: poolGroup,
              members: [],
              eligibleMemberCount: 0,
            }
          }
          error={
            poolError ? providerGroupErrorMessage(poolError, t) : undefined
          }
          onOpenChange={(open) => !open && setPoolGroup(null)}
          onSave={(policy) => {
            setPolicy.mutate(
              { groupId: poolGroup.id, policy },
              { onSuccess: () => setPoolGroup(null) },
            );
          }}
          pending={
            poolLoading ||
            Boolean(poolError) ||
            setPolicy.isPending ||
            setMemberEnabled.isPending ||
            reorderMembers.isPending
          }
          onMemberMove={(providerId, offset) => {
            if (!poolStatus) return;
            const ids = poolStatus.members.map((member) => member.providerId);
            const from = ids.indexOf(providerId);
            const to = from + offset;
            if (from < 0 || to < 0 || to >= ids.length) return;
            ids.splice(to, 0, ids.splice(from, 1)[0]);
            reorderMembers.mutate({ groupId: poolGroup.id, providerIds: ids });
          }}
          onMemberEnabledChange={(providerId, enabled) =>
            setMemberEnabled.mutate({ providerId, enabled })
          }
        />
      )}
      <BalanceQueryTemplateDialog
        open={templateDialog !== undefined}
        appId={appId}
        pending={saveBalanceTemplate.isPending || updateGroup.isPending}
        template={templateDialog}
        onOpenChange={(open) => {
          if (!open) {
            setTemplateDialog(undefined);
            setTemplateTargetGroupId(undefined);
          }
        }}
        onSubmit={(template) => {
          saveBalanceTemplate.mutate(template, {
            onSuccess: () => {
              setBalances({});
              const target = groups.find(
                (group) => group.id === templateTargetGroupId,
              );
              if (target) {
                updateGroup.mutate(
                  {
                    ...target,
                    balanceTemplateId: template.id,
                  },
                  {
                    onSuccess: () => {
                      setTemplateDialog(undefined);
                      setTemplateTargetGroupId(undefined);
                    },
                  },
                );
              } else {
                setTemplateDialog(undefined);
                setTemplateTargetGroupId(undefined);
              }
            },
          });
        }}
      />
    </div>
  );
}
