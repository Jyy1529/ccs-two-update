import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ProviderGroup } from "@/types";
import type { AppId } from "@/lib/api";
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
import { providerGroupIcons } from "./providerGroupIcons";

export interface ProviderGroupSubmit {
  appType: AppId;
  groupId?: string;
  name: string;
  icon: string | null;
  iconColor: string | null;
}

interface ProviderGroupDialogProps {
  open: boolean;
  appId: AppId;
  group?: ProviderGroup | null;
  onOpenChange: (open: boolean) => void;
  onSubmit: (value: ProviderGroupSubmit) => void;
  pending?: boolean;
}

export function ProviderGroupDialog({
  open,
  appId,
  group,
  onOpenChange,
  onSubmit,
  pending = false,
}: ProviderGroupDialogProps) {
  const { t } = useTranslation();
  const [name, setName] = useState(group?.name ?? "");
  const [icon, setIcon] = useState(group?.icon ?? "folder");
  const [iconColor, setIconColor] = useState(group?.iconColor ?? "#eab308");

  useEffect(() => {
    if (open) {
      setName(group?.name ?? "");
      setIcon(group?.icon ?? "folder");
      setIconColor(group?.iconColor ?? "#eab308");
    }
  }, [group, open]);

  const trimmedName = name.trim();
  const isEditing = Boolean(group);
  const Icon =
    providerGroupIcons[icon as keyof typeof providerGroupIcons] ??
    providerGroupIcons.folder;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent zIndex="top">
        <DialogHeader>
          <DialogTitle>
            {isEditing
              ? t("providerGroups.renameTitle", {
                  defaultValue: "Rename folder",
                })
              : t("providerGroups.createTitle", {
                  defaultValue: "Create folder",
                })}
          </DialogTitle>
          <DialogDescription>
            {t("providerGroups.nameDescription", {
              defaultValue: "Organize providers for this application.",
            })}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2 px-6 py-5">
          <Label htmlFor="provider-group-name">
            {t("providerGroups.folderName", { defaultValue: "Folder name" })}
          </Label>
          <Input
            id="provider-group-name"
            value={name}
            maxLength={120}
            onChange={(event) => setName(event.target.value)}
            autoFocus
          />
          <div className="grid grid-cols-2 gap-4 pt-3">
            <div className="space-y-2">
              <Label htmlFor="provider-group-icon">
                {t("providerGroups.folderIcon", {
                  defaultValue: "Folder icon",
                })}
              </Label>
              <select
                id="provider-group-icon"
                value={icon}
                onChange={(event) => setIcon(event.target.value)}
                className="h-9 w-full rounded-md border border-border-default bg-background px-2 text-sm"
              >
                {Object.keys(providerGroupIcons).map((value) => (
                  <option key={value} value={value}>
                    {t(`providerGroups.icons.${value}`, {
                      defaultValue: value,
                    })}
                  </option>
                ))}
              </select>
            </div>
            <div className="space-y-2">
              <Label htmlFor="provider-group-color">
                {t("providerGroups.folderColor", {
                  defaultValue: "Folder color",
                })}
              </Label>
              <div className="flex items-center gap-3">
                <Input
                  id="provider-group-color"
                  type="color"
                  value={iconColor}
                  onChange={(event) => setIconColor(event.target.value)}
                  className="w-16 p-1"
                />
                <Icon
                  className="h-6 w-6"
                  style={{ color: iconColor }}
                  aria-hidden="true"
                />
              </div>
            </div>
          </div>
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
            disabled={!trimmedName || pending}
            onClick={() =>
              onSubmit({
                appType: appId,
                groupId: group?.id,
                name: trimmedName,
                icon: icon === "folder" ? null : icon,
                iconColor: iconColor === "#eab308" ? null : iconColor,
              })
            }
          >
            {t("common.save", { defaultValue: "Save" })}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
