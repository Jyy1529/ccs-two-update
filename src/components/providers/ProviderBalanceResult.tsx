import { Loader2, WalletCards } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { BalanceQueryResult } from "@/types";
import { Button } from "@/components/ui/button";

export function ProviderBalanceResult({
  result,
}: {
  result?: BalanceQueryResult;
}) {
  const { t } = useTranslation();
  if (result?.status !== "success") return null;
  return (
    <div
      className="mt-2 text-right text-xs text-muted-foreground"
      aria-live="polite"
    >
      {result.data.map((item, index) => (
        <div key={index} className="break-words">
          {item.planName ? `${item.planName}: ` : ""}
          {item.remaining ?? "—"} {item.unit ?? ""}
          {item.isValid === false && (
            <span className="ml-2 text-amber-600">
              {t("providerGroups.balanceUnavailable", {
                defaultValue: "Unavailable quota",
              })}
            </span>
          )}
        </div>
      ))}
    </div>
  );
}

export function ProviderBalanceButton({
  name,
  pending,
  disabled,
  onQuery,
}: {
  name: string;
  pending: boolean;
  disabled: boolean;
  onQuery: () => void;
}) {
  const { t } = useTranslation();
  const label = t("providerGroups.queryKeyBalance", {
    member: name,
    defaultValue: "Query balance for {{member}}",
  });
  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      className="h-8 w-8 shrink-0"
      disabled={disabled}
      onClick={onQuery}
      aria-label={label}
      title={label}
    >
      {pending ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" />
      ) : (
        <WalletCards className="h-3.5 w-3.5" />
      )}
    </Button>
  );
}
