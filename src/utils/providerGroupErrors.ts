import type { TFunction } from "i18next";
import { extractErrorMessage } from "./errorUtils";

export function providerGroupErrorMessage(
  error: unknown,
  t: TFunction,
): string {
  const message = extractErrorMessage(error);
  const code = message.match(/\[((?:pool|balance)_[a-z_]+)\]/)?.[1];
  if (code)
    return t(`providerGroups.errors.${code}`, {
      defaultValue: t("providerGroups.operationFailed", {
        defaultValue: "Folder operation failed",
      }),
    });
  const status = message.match(/^HTTP (\d{3})$/)?.[1];
  if (status)
    return t("providerGroups.balanceHttpError", {
      status,
      defaultValue: "Balance request failed (HTTP {{status}})",
    });
  return (
    message ||
    t("providerGroups.operationFailed", {
      defaultValue: "Folder operation failed",
    })
  );
}
