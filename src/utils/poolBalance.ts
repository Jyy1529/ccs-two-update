import type { BalanceQueryResult } from "@/types";

export function getPoolBalanceTotal(
  results: BalanceQueryResult[],
  memberIds: string[],
): { remaining: number; unit: string; currency: string | null } | null {
  if (!memberIds.length || new Set(memberIds).size !== memberIds.length)
    return null;
  const members = memberIds.map((id) =>
    results.filter((result) => result.providerId === id),
  );
  if (members.some((matches) => matches.length !== 1)) return null;
  const values = members.map(([result]) => result);
  const first = values[0];
  const unit = first.data[0]?.unit?.trim();
  const currency = first.currency?.trim().toUpperCase() || null;
  if (!first.aggregationKey || !unit) return null;
  let remaining = 0;
  for (const result of values) {
    const item = result.data[0];
    if (
      result.status !== "success" ||
      result.data.length !== 1 ||
      item.isValid === false ||
      result.aggregationKey !== first.aggregationKey ||
      item.unit?.trim() !== unit ||
      (result.currency?.trim().toUpperCase() || null) !== currency ||
      typeof item.remaining !== "number" ||
      !Number.isFinite(item.remaining)
    )
      return null;
    remaining += item.remaining;
  }
  return Number.isFinite(remaining) ? { remaining, unit, currency } : null;
}
