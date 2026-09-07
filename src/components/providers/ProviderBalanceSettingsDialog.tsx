import { useId, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { AppId } from "@/lib/api";
import { providerGroupsApi } from "@/lib/api/providerGroups";
import {
  useBalanceQueryTemplatesQuery,
  useSaveBalanceQueryTemplateMutation,
} from "@/lib/query/providerGroups";
import type { BalanceQueryTemplate, Provider, ProviderGroup } from "@/types";
import { providerGroupErrorMessage } from "@/utils/providerGroupErrors";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { BalanceQueryTemplateDialog } from "./BalanceQueryTemplateDialog";

export function ProviderBalanceSettingsDialog({
  appId,
  provider,
  group,
  onOpenChange,
  onChanged,
}: {
  appId: AppId;
  provider: Provider;
  group?: ProviderGroup;
  onOpenChange: (open: boolean) => void;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const id = useId();
  const queryClient = useQueryClient();
  const templates = useBalanceQueryTemplatesQuery();
  const saveTemplate = useSaveBalanceQueryTemplateMutation();
  const [templateId, setTemplateId] = useState(
    provider.meta?.balanceTemplateId ?? "",
  );
  const [editing, setEditing] = useState<BalanceQueryTemplate | null>();
  const selected = templates.data?.find(
    (template) => template.id === templateId,
  );
  const binding = useMutation({
    mutationFn: (next: string | null) =>
      providerGroupsApi.setProviderBalanceTemplate(appId, provider.id, next),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["providers", appId] });
      onChanged();
      toast.success(t("providerGroups.providerBalanceSaved"));
      onOpenChange(false);
    },
    onError: (error) => toast.error(providerGroupErrorMessage(error, t)),
  });
  const pending = binding.isPending || saveTemplate.isPending;

  return (
    <>
      <Dialog open onOpenChange={(open) => !pending && onOpenChange(open)}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>
              {t("providerGroups.balanceSettingsFor", {
                provider: provider.name,
              })}
            </DialogTitle>
            <DialogDescription>
              {t("providerGroups.providerBalanceHint")}
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3 px-6 py-4">
            <Label htmlFor={id}>{t("providerGroups.balanceTemplate")}</Label>
            <select
              id={id}
              className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
              value={templateId}
              disabled={pending || templates.isPending || templates.isError}
              onChange={(event) => setTemplateId(event.target.value)}
            >
              <option value="">
                {group
                  ? t("providerGroups.inheritBalanceTemplate", {
                      group: group.name,
                    })
                  : t("providerGroups.builtinDetection")}
              </option>
              {templateId && !selected && (
                <option value={templateId} disabled>
                  {t("providerGroups.errors.balance_template_missing")}
                </option>
              )}
              {templates.data?.map((template) => (
                <option key={template.id} value={template.id}>
                  {template.name}
                </option>
              ))}
            </select>
            {templates.isError && (
              <div role="alert" className="text-sm text-destructive">
                {providerGroupErrorMessage(templates.error, t)}
                <Button
                  type="button"
                  variant="link"
                  onClick={() => void templates.refetch()}
                >
                  {t("common.retry")}
                </Button>
              </div>
            )}
            <p className="text-xs text-muted-foreground">
              {t("providerGroups.providerBalanceCredentialsHint")}
            </p>
            <div className="flex gap-2">
              <Button
                type="button"
                variant="outline"
                disabled={pending}
                onClick={() => setEditing(null)}
              >
                {t("providerGroups.newBalanceTemplate")}
              </Button>
              <Button
                type="button"
                variant="outline"
                disabled={pending || !selected}
                onClick={() => setEditing(selected)}
              >
                {t("providerGroups.editBalanceTemplate")}
              </Button>
            </div>
          </div>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              disabled={pending}
              onClick={() => onOpenChange(false)}
            >
              {t("common.cancel")}
            </Button>
            <Button
              type="button"
              disabled={
                pending ||
                templates.isPending ||
                templates.isError ||
                Boolean(templateId && !selected)
              }
              onClick={() => binding.mutate(templateId || null)}
            >
              {pending ? t("common.saving") : t("common.save")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <BalanceQueryTemplateDialog
        open={editing !== undefined}
        appId={appId}
        template={editing}
        pending={pending}
        onOpenChange={(open) => !open && setEditing(undefined)}
        onSubmit={(template) =>
          saveTemplate.mutate(template, {
            onSuccess: () => {
              onChanged();
              setTemplateId(template.id);
              setEditing(undefined);
              binding.mutate(template.id);
            },
          })
        }
      />
    </>
  );
}
