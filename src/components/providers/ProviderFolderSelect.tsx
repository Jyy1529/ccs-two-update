import { useTranslation } from "react-i18next";
import { Folder } from "lucide-react";
import type { Provider, ProviderGroup } from "@/types";

interface ProviderFolderSelectProps {
  provider: Provider;
  groups: ProviderGroup[];
  groupId?: string;
  disabled: boolean;
  onChange: (groupId: string | null) => void;
}

export function ProviderFolderSelect({
  provider,
  groups,
  groupId,
  disabled,
  onChange,
}: ProviderFolderSelectProps) {
  const { t } = useTranslation();
  const id = `provider-folder-${provider.id}`;
  return (
    <div className="flex min-w-0 items-center gap-1 text-muted-foreground">
      <Folder className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
      <label htmlFor={id} className="sr-only">
        {t("providerGroups.folderLabel", { defaultValue: "Folder" })}
      </label>
      <select
        id={id}
        disabled={disabled}
        value={groupId ?? ""}
        title={
          !groupId
            ? t("providerGroups.noFolderDescription", {
                defaultValue:
                  "No folder only means this Provider is not in a folder; it remains available on its own.",
              })
            : undefined
        }
        aria-label={t("providerGroups.folderFor", {
          provider: provider.name,
          defaultValue: "Folder for {{provider}}",
        })}
        className="h-8 max-w-32 rounded-md border border-border-default bg-background px-2 text-xs disabled:opacity-50"
        onChange={(event) => onChange(event.target.value || null)}
      >
        <option value="">
          {t("providerGroups.noFolder", { defaultValue: "No folder" })}
        </option>
        {groups.map((group) => (
          <option key={group.id} value={group.id}>
            {group.name}
          </option>
        ))}
      </select>
    </div>
  );
}
