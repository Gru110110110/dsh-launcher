import { describe, expect, it } from "vitest";
import {
  catalogGenerationChanged,
  compatibilityPresentation,
  formatScore,
  formatStars,
  installedFilterValue,
  isForceableCompatibilityError,
  isMarketCatalogUnavailable,
  isRetryableMarketRefreshError,
  marketCatalogView,
  marketConflictDetail,
  paginationItems,
  pendingChangeLabels,
  shouldClearPendingVerification,
} from "./presentation";

describe("catalogGenerationChanged", () => {
  it("keeps the refresh-start baseline when a revision event wins the race", () => {
    const refreshStartedAt = "old-generation";
    let observedGeneration = refreshStartedAt;

    observedGeneration = "new-generation";

    expect(catalogGenerationChanged(refreshStartedAt, observedGeneration)).toBe(
      true,
    );
  });

  it("does not report an unchanged generation", () => {
    expect(catalogGenerationChanged("same-generation", "same-generation")).toBe(
      false,
    );
  });
});

describe("marketCatalogView", () => {
  it("shows the initial loading layout until the first page is available", () => {
    expect(marketCatalogView(null, false)).toBe("loading");
    expect(marketCatalogView("loading", false)).toBe("loading");
    expect(marketCatalogView("ready", false)).toBe("loading");
  });

  it("shows the failure layout only when there is no cached page", () => {
    expect(marketCatalogView("failed", false)).toBe("failed");
    expect(marketCatalogView("failed", true)).toBe("content");
  });

  it("keeps cached content visible while a refresh is loading", () => {
    expect(marketCatalogView("loading", true)).toBe("content");
  });
});

describe("compatibilityPresentation", () => {
  it("maps every status to a label and tone", () => {
    expect(compatibilityPresentation("compatible")).toEqual({
      labelKey: "market.compat.compatible",
      tone: "ok",
    });
    expect(compatibilityPresentation("incompatible")).toEqual({
      labelKey: "market.compat.incompatible",
      tone: "warn",
    });
    expect(compatibilityPresentation("unknown")).toEqual({
      labelKey: "market.compat.unknown",
      tone: "caution",
    });
    expect(compatibilityPresentation("notChecked")).toEqual({
      labelKey: "market.compat.notChecked",
      tone: "unknown",
    });
  });
});

describe("shouldClearPendingVerification", () => {
  it("keeps a marker while the currently ready service predates installation", () => {
    expect(shouldClearPendingVerification("ready", 100, 200)).toBe(false);
  });

  it("clears a marker only after a newer service start succeeds", () => {
    expect(shouldClearPendingVerification("starting", 300, 200)).toBe(false);
    expect(shouldClearPendingVerification("ready", 300, 200)).toBe(true);
  });
});

describe("pendingChangeLabels", () => {
  const pending = (
    changes: Array<{
      pluginId: string;
      name: string;
      profile: string | null;
    }>,
  ) => ({
    pluginId: changes[changes.length - 1]?.pluginId ?? "",
    name: changes[changes.length - 1]?.name ?? "",
    installedAtMs: 1,
    changes: changes.map((change) => ({
      ...change,
      action: "install" as const,
    })),
    journalRecovered: false,
  });

  it("uses the plugin name when the identity is known", () => {
    expect(
      pendingChangeLabels(
        pending([{ pluginId: "x/alpha", name: "alpha", profile: "web" }]),
      ),
    ).toEqual(["alpha"]);
  });

  it("falls back to the profile name for recovered entries", () => {
    expect(
      pendingChangeLabels(
        pending([{ pluginId: "", name: "web", profile: "web" }]),
      ),
    ).toEqual(["web"]);
  });

  it("labels recovered and newly added entries independently", () => {
    expect(
      pendingChangeLabels(
        pending([
          { pluginId: "", name: "web", profile: "web" },
          { pluginId: "x/beta", name: "beta", profile: "web" },
        ]),
      ),
    ).toEqual(["web", "beta"]);
  });
});

describe("formatters", () => {
  it("formats stars compactly", () => {
    expect(formatStars(42)).toBe("42");
    expect(formatStars(999)).toBe("999");
    expect(formatStars(1234)).toBe("1.2k");
    expect(formatStars(10_000)).toBe("10.0k");
  });

  it("formats scores", () => {
    expect(formatScore(null)).toBe("–");
    expect(formatScore(87.4)).toBe("87");
  });
});

describe("isMarketCatalogUnavailable", () => {
  it("recognizes the not-ready-yet code used during the first download", () => {
    expect(
      isMarketCatalogUnavailable({ code: "marketCatalogUnavailable" }),
    ).toBe(true);
    expect(isMarketCatalogUnavailable({ code: "marketNetworkFailed" })).toBe(
      false,
    );
    expect(isMarketCatalogUnavailable(null)).toBe(false);
  });
});

describe("isRetryableMarketRefreshError", () => {
  it("retries only transient marketplace service failures", () => {
    expect(
      isRetryableMarketRefreshError({
        code: "marketCatalogServiceUnavailable",
        values: { retryable: "true" },
      }),
    ).toBe(true);
    expect(
      isRetryableMarketRefreshError({
        code: "marketCatalogServiceUnavailable",
        values: { retryable: "false" },
      }),
    ).toBe(false);
    expect(
      isRetryableMarketRefreshError({ code: "marketCatalogInvalid" }),
    ).toBe(false);
  });
});

describe("isForceableCompatibilityError", () => {
  it("allows only compatibility decisions to be force-confirmed", () => {
    expect(isForceableCompatibilityError({ code: "marketIncompatible" })).toBe(
      true,
    );
    expect(isForceableCompatibilityError({ code: "marketCompatUnknown" })).toBe(
      true,
    );
    expect(
      isForceableCompatibilityError({ code: "marketSourceMismatch" }),
    ).toBe(false);
    expect(isForceableCompatibilityError({ code: "marketSourceUnknown" })).toBe(
      false,
    );
  });

  it("extracts only a non-empty safe conflict detail", () => {
    expect(marketConflictDetail({ safeDetail: "requires cordis ^5" })).toBe(
      "requires cordis ^5",
    );
    expect(marketConflictDetail({ safeDetail: "" })).toBeUndefined();
    expect(marketConflictDetail(null)).toBeUndefined();
  });
});

describe("installedFilterValue", () => {
  it("maps the three-way filter to the query tri-state", () => {
    expect(installedFilterValue("")).toBe(null);
    expect(installedFilterValue("installed")).toBe(true);
    expect(installedFilterValue("notInstalled")).toBe(false);
  });
});

describe("paginationItems", () => {
  it("shows every page when the result set is short", () => {
    expect(paginationItems(3, 5)).toEqual([1, 2, 3, 4, 5]);
  });

  it("keeps the first, last, and nearby pages visible", () => {
    expect(paginationItems(1, 206)).toEqual([
      1,
      2,
      3,
      4,
      5,
      "end-ellipsis",
      206,
    ]);
    expect(paginationItems(103, 206)).toEqual([
      1,
      "start-ellipsis",
      102,
      103,
      104,
      "end-ellipsis",
      206,
    ]);
    expect(paginationItems(206, 206)).toEqual([
      1,
      "start-ellipsis",
      202,
      203,
      204,
      205,
      206,
    ]);
  });

  it("clamps an out-of-range current page", () => {
    expect(paginationItems(999, 9)).toEqual([
      1,
      "start-ellipsis",
      5,
      6,
      7,
      8,
      9,
    ]);
  });
});
