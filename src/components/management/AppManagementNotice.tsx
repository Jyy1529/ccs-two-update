import { useTranslation } from "react-i18next";
import { AlertTriangle, ShieldOff } from "lucide-react";
import { useAppManagement } from "@/lib/query/appManagement";
import type { AppId } from "@/lib/api/types";
import { Button } from "@/components/ui/button";

export function AppManagementNotice({ appId }: { appId: AppId }) {
  const { t } = useTranslation();
  const management = useAppManagement(appId);
  if (management.canWrite) return null;

  return (
    <div
      role={management.isError ? "alert" : "status"}
      className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm"
    >
      {management.isError ? (
        <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
      ) : (
        <ShieldOff className="mt-0.5 h-4 w-4 shrink-0" />
      )}
      <div className="min-w-0 flex-1 space-y-1">
        <p className="font-medium">
          {t(
            management.isError
              ? "appManagement.unavailable"
              : management.entry
                ? `appManagement.phases.${management.entry.phase}`
                : "appManagement.loading",
          )}
        </p>
        <p className="text-xs text-muted-foreground">
          {t("appManagement.databaseOnly")}
        </p>
        {management.entry?.message && (
          <p className="break-words text-xs">{management.entry.message}</p>
        )}
      </div>
      {management.isError && (
        <Button
          size="sm"
          variant="outline"
          onClick={() => void management.refetch()}
        >
          {t("common.retry")}
        </Button>
      )}
    </div>
  );
}
