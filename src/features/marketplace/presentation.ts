import type {
  CompatibilityStatus,
  LauncherPhase,
  MarketCatalogState,
  MarketSort,
  PendingVerification,
  PluginKind,
} from "@/platform/generated/bindings";

export const MARKET_PAGE_SIZE = 24;

export function catalogGenerationChanged(
  refreshStartedAt: string | null,
  latest: string | null,
): boolean {
  return latest !== refreshStartedAt;
}

type MarketCatalogView = "loading" | "failed" | "content";

export function marketCatalogView(
  catalogKind: MarketCatalogState["kind"] | null,
  hasData: boolean,
): MarketCatalogView {
  if (hasData) return "content";
  return catalogKind === "failed" ? "failed" : "loading";
}

type PaginationItem = number | "start-ellipsis" | "end-ellipsis";

export function paginationItems(
  currentPage: number,
  totalPages: number,
): PaginationItem[] {
  const total = Math.max(1, Math.floor(totalPages));
  const current = Math.min(total, Math.max(1, Math.floor(currentPage)));

  if (total <= 7) {
    return Array.from({ length: total }, (_, index) => index + 1);
  }

  if (current <= 4) {
    return [1, 2, 3, 4, 5, "end-ellipsis", total];
  }

  if (current >= total - 3) {
    return [
      1,
      "start-ellipsis",
      total - 4,
      total - 3,
      total - 2,
      total - 1,
      total,
    ];
  }

  return [
    1,
    "start-ellipsis",
    current - 1,
    current,
    current + 1,
    "end-ellipsis",
    total,
  ];
}

export function shouldClearPendingVerification(
  phase: LauncherPhase,
  serviceStartedAtMs: number | null,
  installedAtMs: number,
): boolean {
  return (
    phase === "ready" &&
    serviceStartedAtMs !== null &&
    serviceStartedAtMs > installedAtMs
  );
}

/// Display label for one pending batch entry. Entries whose identity was lost
/// with a damaged journal fall back to the profile name; entries written
/// after the recovery keep their plugin name, so a mixed batch labels each
/// entry with the best identity it actually has.
export function pendingChangeLabels(pending: PendingVerification): string[] {
  return pending.changes.map((change) =>
    change.pluginId === "" ? (change.profile ?? change.name) : change.name,
  );
}

type CompatibilityTone = "ok" | "warn" | "caution" | "unknown";

type CompatibilityPresentation = {
  labelKey: string;
  tone: CompatibilityTone;
};

export function compatibilityPresentation(
  status: CompatibilityStatus,
): CompatibilityPresentation {
  switch (status) {
    case "compatible":
      return { labelKey: "market.compat.compatible", tone: "ok" };
    case "incompatible":
      return { labelKey: "market.compat.incompatible", tone: "warn" };
    case "unknown":
      return { labelKey: "market.compat.unknown", tone: "caution" };
    default:
      return { labelKey: "market.compat.notChecked", tone: "unknown" };
  }
}

export function formatStars(stars: number): string {
  if (stars >= 1000) return `${(stars / 1000).toFixed(1)}k`;
  return String(stars);
}

export function formatScore(score: number | null): string {
  if (score === null) return "–";
  return String(Math.round(score));
}

type SortOption = { value: MarketSort; labelKey: string };

export const SORT_OPTIONS: readonly SortOption[] = [
  { value: "score", labelKey: "market.sort.score" },
  { value: "stars", labelKey: "market.sort.stars" },
  { value: "recentlyUpdated", labelKey: "market.sort.recentlyUpdated" },
  { value: "name", labelKey: "market.sort.name" },
];

type KindFilter = "" | PluginKind;

type KindOption = { value: KindFilter; labelKey: string };

export const KIND_OPTIONS: readonly KindOption[] = [
  { value: "", labelKey: "market.kind.all" },
  { value: "cordisPlugin", labelKey: "market.kind.cordisPlugin" },
  { value: "skill", labelKey: "market.kind.skill" },
];

export type InstalledFilter = "" | "installed" | "notInstalled";

type InstalledOption = { value: InstalledFilter; labelKey: string };

export const INSTALLED_OPTIONS: readonly InstalledOption[] = [
  { value: "", labelKey: "market.installed.all" },
  { value: "installed", labelKey: "market.installed.installed" },
  { value: "notInstalled", labelKey: "market.installed.notInstalled" },
];

export function installedFilterValue(filter: InstalledFilter): boolean | null {
  if (filter === "") return null;
  return filter === "installed";
}

export function isMarketCatalogUnavailable(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  return (error as { code?: unknown }).code === "marketCatalogUnavailable";
}
