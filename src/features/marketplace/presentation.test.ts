import { describe, expect, it } from "vitest";
import {
  compatibilityPresentation,
  formatScore,
  formatStars,
  installedFilterValue,
  isMarketCatalogUnavailable,
  isMarketConflictError,
  marketConflictDetail,
  paginationItems,
  shouldClearPendingVerification,
} from "./presentation";

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

describe("isMarketConflictError", () => {
  it("recognizes the codes that require user confirmation", () => {
    expect(isMarketConflictError({ code: "marketIncompatible" })).toBe(true);
    expect(isMarketConflictError({ code: "marketCompatUnknown" })).toBe(true);
    expect(isMarketConflictError({ code: "marketSourceMismatch" })).toBe(true);
    expect(isMarketConflictError({ code: "marketSourceUnknown" })).toBe(true);
    expect(isMarketConflictError({ code: "marketInstallFailed" })).toBe(false);
    expect(isMarketConflictError("boom")).toBe(false);
    expect(isMarketConflictError(null)).toBe(false);
  });
});

describe("marketConflictDetail", () => {
  it("extracts the safe detail string when present", () => {
    expect(
      marketConflictDetail({
        code: "marketIncompatible",
        safeDetail: "requires cordis ^4.0.0, installed 4.0.1",
      }),
    ).toBe("requires cordis ^4.0.0, installed 4.0.1");
    expect(marketConflictDetail({ code: "marketCompatUnknown" })).toBe(
      undefined,
    );
    expect(marketConflictDetail({ safeDetail: 42 })).toBe(undefined);
    expect(marketConflictDetail(null)).toBe(undefined);
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
