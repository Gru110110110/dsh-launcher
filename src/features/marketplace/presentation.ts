import type {
  CompatibilityStatus,
  LauncherPhase,
  MarketSort,
  PluginKind,
} from "@/platform/generated/bindings";

export const MARKET_PAGE_SIZE = 24;

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

export function isMarketConflictError(
  error: unknown,
): error is { code: string } {
  if (typeof error !== "object" || error === null) return false;
  const code = (error as { code?: unknown }).code;
  return (
    code === "marketIncompatible" ||
    code === "marketCompatUnknown" ||
    code === "marketSourceMismatch" ||
    code === "marketSourceUnknown"
  );
}

export function isMarketCatalogUnavailable(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  return (error as { code?: unknown }).code === "marketCatalogUnavailable";
}

export function marketConflictDetail(error: unknown): string | undefined {
  if (typeof error !== "object" || error === null) return undefined;
  const detail = (error as { safeDetail?: unknown }).safeDetail;
  return typeof detail === "string" && detail.length > 0 ? detail : undefined;
}
