import {
  ChevronDown,
  ChevronRight,
  Folder,
  GripVertical,
  ArrowUp,
  ArrowDown,
  KeyRound,
  MoreHorizontal,
  Pencil,
  Trash2,
  WalletCards,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import type { ReactNode } from "react";
import { useDndContext } from "@dnd-kit/core";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { providerGroupIcons } from "./providerGroupIcons";
import type { ProviderGroup } from "@/types";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";

export interface ProviderGroupHeaderProps {
  group: ProviderGroup;
  memberCount: number;
  onToggleCollapsed: () => void;
  onRename: () => void;
  onDelete: () => void;
  onConfigurePool: () => void;
  onQueryBalances: () => void;
  isQueryingBalances?: boolean;
  balanceTemplateControls?: ReactNode;
  onMoveUp?: () => void;
  onMoveDown?: () => void;
  dragHandleProps?: Pick<
    ReturnType<typeof useSortable>,
    "attributes" | "listeners" | "setActivatorNodeRef" | "isDragging"
  > & { disabled: boolean };
}

export function SortableProviderGroup({
  children,
  dragDisabled,
  ...headerProps
}: Omit<ProviderGroupHeaderProps, "dragHandleProps"> & {
  children: ReactNode;
  dragDisabled: boolean;
}) {
  const { active } = useDndContext();
  const sortable = useSortable({
    id: `provider-group:${headerProps.group.id}`,
    data: {
      type: "provider-group",
      groupId: headerProps.group.id,
      appId: headerProps.group.appType,
    },
    disabled: {
      draggable: dragDisabled,
      droppable:
        active !== null && active.data.current?.type !== "provider-group",
    },
  });
  return (
    <section
      ref={sortable.setNodeRef}
      aria-label={headerProps.group.name}
      style={{
        transform: CSS.Transform.toString(sortable.transform),
        transition: sortable.transition,
      }}
      className={cn(
        "relative overflow-hidden rounded-lg border border-border-default bg-muted/10",
        sortable.isDragging && "z-10 opacity-80 shadow-lg",
      )}
    >
      <ProviderGroupHeader
        {...headerProps}
        dragHandleProps={{
          attributes: sortable.attributes,
          listeners: sortable.listeners,
          setActivatorNodeRef: sortable.setActivatorNodeRef,
          isDragging: sortable.isDragging,
          disabled: dragDisabled,
        }}
      />
      {children}
    </section>
  );
}

export function ProviderGroupHeader({
  group,
  memberCount,
  onToggleCollapsed,
  onRename,
  onDelete,
  onConfigurePool,
  onQueryBalances,
  isQueryingBalances = false,
  balanceTemplateControls,
  onMoveUp,
  onMoveDown,
  dragHandleProps,
}: ProviderGroupHeaderProps) {
  const { t } = useTranslation();
  const Icon =
    providerGroupIcons[group.icon as keyof typeof providerGroupIcons] ?? Folder;
  const strategy = group.keyPoolEnabled
    ? group.keyPoolStrategy === "failover"
      ? t("providerGroups.failover", { defaultValue: "Failover" })
      : t("providerGroups.roundRobin", { defaultValue: "Round robin" })
    : t("providerGroups.poolDisabled", { defaultValue: "disabled" });

  return (
    <div className="flex min-w-0 flex-wrap items-center gap-2 border-b border-border-default px-3 py-2">
      {dragHandleProps && (
        <Button
          ref={dragHandleProps.setActivatorNodeRef}
          type="button"
          variant="ghost"
          size="icon"
          className={cn(
            "h-7 w-5 shrink-0 touch-none cursor-grab",
            dragHandleProps.isDragging && "cursor-grabbing",
          )}
          {...dragHandleProps.attributes}
          {...dragHandleProps.listeners}
          disabled={dragHandleProps.disabled}
          aria-label={t("providerGroups.dragFolder", {
            group: group.name,
            defaultValue: "Drag folder {{group}}",
          })}
        >
          <GripVertical className="h-4 w-4" />
        </Button>
      )}
      <Button
        type="button"
        size="icon"
        variant="ghost"
        className="h-7 w-7 shrink-0"
        aria-label={t("providerGroups.toggle", {
          defaultValue: "Toggle folder",
        })}
        onClick={onToggleCollapsed}
        aria-expanded={!group.collapsed}
      >
        {group.collapsed ? (
          <ChevronRight className="h-4 w-4" />
        ) : (
          <ChevronDown className="h-4 w-4" />
        )}
      </Button>
      <Icon
        className="h-4 w-4 shrink-0"
        style={{ color: group.iconColor ?? "#eab308" }}
      />
      <span className="min-w-0 flex-1 truncate font-medium">{group.name}</span>
      {balanceTemplateControls}
      <span className="shrink-0 text-xs text-muted-foreground">
        {memberCount}
      </span>
      <span
        className={cn(
          "hidden shrink-0 items-center gap-1 text-xs sm:inline-flex",
          group.keyPoolEnabled
            ? "text-emerald-600 dark:text-emerald-400"
            : "text-muted-foreground",
        )}
      >
        <KeyRound className="h-3.5 w-3.5" />
        {strategy}
      </span>
      <Button
        type="button"
        size="sm"
        variant="ghost"
        className="h-7 shrink-0 gap-1 px-2"
        aria-label={t("providerGroups.configurePool", {
          defaultValue: "Key pool",
        })}
        title={t("providerGroups.configurePool", {
          defaultValue: "Key pool",
        })}
        onClick={onConfigurePool}
      >
        <KeyRound className="h-4 w-4" />
        <span className="hidden sm:inline">
          {t("providerGroups.configurePool", {
            defaultValue: "Key pool",
          })}
        </span>
      </Button>
      <Button
        type="button"
        size="icon"
        variant="ghost"
        className="h-7 w-7 shrink-0"
        aria-label={t("providerGroups.queryBalances", {
          defaultValue: "Query balances",
        })}
        title={t("providerGroups.queryBalances", {
          defaultValue: "Query balances",
        })}
        disabled={isQueryingBalances}
        onClick={onQueryBalances}
      >
        <WalletCards className="h-4 w-4" />
      </Button>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            size="icon"
            variant="ghost"
            className="h-7 w-7 shrink-0"
            aria-label={t("providerGroups.menu", {
              defaultValue: "Folder actions",
            })}
          >
            <MoreHorizontal className="h-4 w-4" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onSelect={onMoveUp} disabled={!onMoveUp}>
            <ArrowUp className="h-4 w-4" />
            {t("providerGroups.moveUp", { defaultValue: "Move up" })}
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={onMoveDown} disabled={!onMoveDown}>
            <ArrowDown className="h-4 w-4" />
            {t("providerGroups.moveDown", { defaultValue: "Move down" })}
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={onConfigurePool}>
            <KeyRound className="h-4 w-4" />
            {t("providerGroups.configurePool", {
              defaultValue: "Key pool",
            })}
          </DropdownMenuItem>
          <DropdownMenuItem onSelect={onRename}>
            <Pencil className="h-4 w-4" />
            {t("providerGroups.renameTitle", { defaultValue: "Rename folder" })}
          </DropdownMenuItem>
          <DropdownMenuItem
            onSelect={onDelete}
            className="text-red-600 focus:text-red-600 dark:text-red-400"
          >
            <Trash2 className="h-4 w-4" />
            {t("common.delete", { defaultValue: "Delete" })}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
