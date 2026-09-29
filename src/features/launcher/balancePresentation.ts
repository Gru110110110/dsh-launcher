import type { BalanceSnapshot } from "@/platform/generated/bindings";

/** The balance card stays between the service and resource sections. */
type DashboardSection = "service" | "balance" | "resources";

export function getDashboardSections(
  showBalanceCard: boolean,
): DashboardSection[] {
  return showBalanceCard
    ? ["service", "balance", "resources"]
    : ["service", "resources"];
}

/** Preserve the exact decimal string returned by the official API. */
export function formatBalance(
  totalBalance: string | null,
  currency: string | null,
): string | null {
  if (totalBalance === null) return null;
  if (currency === "CNY") return `¥${totalBalance}`;
  return currency === null ? totalBalance : `${totalBalance} ${currency}`;
}

/** Do not let a delayed request replace a more recently fetched snapshot. */
export function selectNewestSnapshot(
  current: BalanceSnapshot | null,
  next: BalanceSnapshot,
): BalanceSnapshot | null {
  if (
    current?.fetchedAtMs !== null &&
    current?.fetchedAtMs !== undefined &&
    next.fetchedAtMs !== null &&
    next.fetchedAtMs < current.fetchedAtMs
  ) {
    return current;
  }
  return next;
}

/** Keep the last known amount when a snapshot carries no balance. */
export function selectNewerBalance(
  current: BalanceSnapshot | null,
  next: BalanceSnapshot,
): BalanceSnapshot | null {
  if (next.totalBalance === null) return current;
  return selectNewestSnapshot(current, next);
}

/**
 * The account needs funding when the official balance is exhausted, or when no
 * DeepSeek API key is configured and no amount is known at all. A known amount
 * decides on its own, so a positive last-known balance is never paired with a
 * top-up button; `isAvailable` alone cannot decide this because DeepSeek also
 * reports `false` for a funded account that may not call the API.
 */
export function balanceNeedsTopUp(balance: BalanceSnapshot | null): boolean {
  if (balance === null) return false;
  const total = balance.totalBalance;
  if (total !== null) {
    const value = Number(total);
    return Number.isFinite(value) && value <= 0;
  }
  return balance.detail === "balanceNoCredential";
}
