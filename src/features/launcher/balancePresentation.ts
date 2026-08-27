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

/** Do not let a delayed request replace a more recently fetched balance. */
export function selectNewerBalance(
  current: BalanceSnapshot | null,
  next: BalanceSnapshot,
): BalanceSnapshot | null {
  if (next.totalBalance === null) return current;
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
