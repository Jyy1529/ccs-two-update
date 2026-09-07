import { describe, expect, it } from "vitest";
import type { BalanceQueryResult } from "@/types";
import { getPoolBalanceTotal } from "@/utils/poolBalance";

const result = (
  id: string,
  remaining: number,
  unit = "USD",
): BalanceQueryResult => ({
  providerId: id,
  providerName: id,
  status: "success",
  currency: "USD",
  aggregationKey: "per-key:template-1",
  data: [{ remaining, unit }],
});

describe("pool balance totals", () => {
  it("sums only enabled pool members with comparable per-key balances", () => {
    const values = [result("a", 10), result("b", 2.5), result("outside", 100)];
    expect(getPoolBalanceTotal(values, ["a", "b"])).toEqual({
      remaining: 12.5,
      unit: "USD",
      currency: "USD",
    });
  });
  it.each([
    "currency",
    "unit",
    "scope",
    "semantic",
    "failed",
    "missing",
    "multiple",
    "nonfinite",
  ])("does not sum %s results", (kind) => {
    const values = [result("a", 10), result("b", 2)];
    if (kind === "currency") values[1].currency = "CNY";
    if (kind === "unit") values[1].data[0].unit = "tokens";
    if (kind === "scope") values[1].aggregationKey = null;
    if (kind === "semantic") values[1].aggregationKey = "another-template";
    if (kind === "failed") values[1].status = "failed";
    if (kind === "missing") values.pop();
    if (kind === "multiple") values[1].data.push({ remaining: 4, unit: "USD" });
    if (kind === "nonfinite") values[1].data[0].remaining = Infinity;
    expect(getPoolBalanceTotal(values, ["a", "b"])).toBeNull();
  });
  it("does not total an empty pool or duplicate result IDs", () => {
    expect(getPoolBalanceTotal([], [])).toBeNull();
    expect(
      getPoolBalanceTotal([result("a", 10), result("a", 10)], ["a"]),
    ).toBeNull();
  });
});
