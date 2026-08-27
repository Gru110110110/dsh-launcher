import { describe, expect, it } from "vitest";
import type { BalanceSnapshot } from "@/platform/generated/bindings";
import {
  formatBalance,
  getDashboardSections,
  selectNewerBalance,
} from "./balancePresentation";

describe("dashboard section order", () => {
  it("inserts the balance card between service and resources", () => {
    expect(getDashboardSections(true)).toEqual([
      "service",
      "balance",
      "resources",
    ]);
    expect(getDashboardSections(false)).toEqual(["service", "resources"]);
  });
});

describe("official balance formatting", () => {
  it("uses the yuan sign for CNY and never converts another currency", () => {
    expect(formatBalance("46.57", "CNY")).toBe("¥46.57");
    expect(formatBalance("5.5", "USD")).toBe("5.5 USD");
    expect(formatBalance(null, "CNY")).toBeNull();
  });

  it("keeps the newest fetched snapshot when requests finish out of order", () => {
    const snapshot = (
      totalBalance: string | null,
      fetchedAtMs: number | null,
    ): BalanceSnapshot => ({
      status: "ok",
      detail: null,
      isAvailable: true,
      currency: "CNY",
      totalBalance,
      fetchedAtMs,
    });
    const current = snapshot("2.00", 200);
    expect(selectNewerBalance(current, snapshot("1.00", 100))).toBe(current);
    expect(selectNewerBalance(current, snapshot("3.00", 300))).toEqual(
      snapshot("3.00", 300),
    );
    expect(selectNewerBalance(current, snapshot(null, null))).toBe(current);
  });
});
