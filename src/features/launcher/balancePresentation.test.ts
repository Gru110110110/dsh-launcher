import { describe, expect, it } from "vitest";
import type { BalanceSnapshot } from "@/platform/generated/bindings";
import {
  balanceNeedsTopUp,
  formatBalance,
  getDashboardSections,
  selectNewerBalance,
  selectNewestSnapshot,
} from "./balancePresentation";

const snapshot = (
  overrides: Partial<BalanceSnapshot> = {},
): BalanceSnapshot => ({
  status: "ok",
  detail: null,
  isAvailable: true,
  currency: "CNY",
  totalBalance: "46.57",
  fetchedAtMs: 100,
  ...overrides,
});

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
    const current = snapshot({ totalBalance: "2.00", fetchedAtMs: 200 });
    expect(
      selectNewerBalance(
        current,
        snapshot({ totalBalance: "1.00", fetchedAtMs: 100 }),
      ),
    ).toBe(current);
    expect(
      selectNewerBalance(
        current,
        snapshot({ totalBalance: "3.00", fetchedAtMs: 300 }),
      ),
    ).toEqual(snapshot({ totalBalance: "3.00", fetchedAtMs: 300 }));
    expect(
      selectNewerBalance(
        current,
        snapshot({ totalBalance: null, fetchedAtMs: null }),
      ),
    ).toBe(current);
  });

  it("classifies an initial missing amount without replacing a newer dated snapshot", () => {
    const current = snapshot({ totalBalance: "2.00", fetchedAtMs: 200 });
    const missing = snapshot({
      totalBalance: null,
      fetchedAtMs: null,
      detail: "balanceNoCredential",
    });
    expect(selectNewestSnapshot(null, missing)).toEqual(missing);
    expect(selectNewestSnapshot(current, missing)).toBe(current);
  });
});

describe("top-up prompt", () => {
  it("asks for a top-up when no API key is configured", () => {
    expect(
      balanceNeedsTopUp(
        snapshot({
          status: "unavailable",
          detail: "balanceNoCredential",
          isAvailable: null,
          currency: null,
          totalBalance: null,
          fetchedAtMs: null,
        }),
      ),
    ).toBe(true);
  });

  it("asks for a top-up when the official balance is exhausted", () => {
    for (const totalBalance of ["0", "0.00", "0.00000000", "-1.00"]) {
      expect(balanceNeedsTopUp(snapshot({ totalBalance }))).toBe(true);
    }
  });

  it("keeps the ordinary refresh action for a funded or unknown balance", () => {
    expect(balanceNeedsTopUp(snapshot({ totalBalance: "0.01" }))).toBe(false);
    expect(
      balanceNeedsTopUp(
        snapshot({
          status: "unavailable",
          detail: "balanceFetchFailed",
          isAvailable: null,
          totalBalance: null,
          fetchedAtMs: null,
        }),
      ),
    ).toBe(false);
    expect(balanceNeedsTopUp(null)).toBe(false);
  });

  it("lets a known amount decide instead of the credential or availability flag", () => {
    // The bridge keeps a positive last-known amount while reporting a missing
    // key, and DeepSeek reports `is_available: false` for a funded account
    // that may not call the API, so neither flag may prompt a top-up alone.
    expect(
      balanceNeedsTopUp(
        snapshot({
          status: "stale",
          detail: "balanceNoCredential",
          totalBalance: "88.38",
        }),
      ),
    ).toBe(false);
    expect(
      balanceNeedsTopUp(snapshot({ isAvailable: false, totalBalance: "9.99" })),
    ).toBe(false);
    expect(
      balanceNeedsTopUp(
        snapshot({
          status: "unavailable",
          detail: "balanceNoCredential",
          totalBalance: null,
          fetchedAtMs: null,
        }),
        snapshot({ totalBalance: "88.38", fetchedAtMs: 200 }),
      ),
    ).toBe(false);
  });
});
