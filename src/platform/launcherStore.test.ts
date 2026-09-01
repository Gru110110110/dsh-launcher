import { afterEach, describe, expect, it } from "vitest";
import type { LauncherSnapshot } from "./generated/bindings";
import { __launcherStoreTest, shallowEqual } from "./launcherStore";

function snapshot(
  revision: number,
  phase: LauncherSnapshot["phase"],
): LauncherSnapshot {
  return {
    revision,
    marketBusy: false,
    marketRevision: 0,
    marketCatalogRevision: 0,
    phase,
    step: "prepare",
    activity: null,
    progress: { kind: "indeterminate" },
    webUrl: null,
    serviceStartedAtMs: null,
    desktopVersion: "0.2.0",
    harnessVersion: null,
    previousHarnessVersion: null,
    removedIncompatiblePlugins: [],
    repairedProjectionCache: false,
    desktopUpdate: { kind: "idle" },
    harnessUpdate: { kind: "none" },
    migration: { kind: "notRequired" },
    browsers: [],
    selectedBrowserId: "system",
    language: "zh",
    theme: "system",
    trayAvailable: false,
    showBalanceCard: true,
    harnessUpdateChannel: "latest",
    proxy: { mode: "system", url: "", bypass: "" },
    remote: {
      master: false,
      serviceReady: false,
      lan: { enabled: false, url: null, password: "" },
      public: {
        enabled: false,
        state: "off",
        url: null,
        password: "",
        error: null,
      },
    },
    error: null,
  };
}

describe("launcher state stream", () => {
  afterEach(() => {
    __launcherStoreTest.reset();
  });

  it("rejects an event older than the current snapshot", () => {
    __launcherStoreTest.accept(snapshot(5, "ready"));
    __launcherStoreTest.accept(snapshot(4, "preparing"));
    expect(__launcherStoreTest.current()?.phase).toBe("ready");
  });

  it("accepts a newer event", () => {
    __launcherStoreTest.accept(snapshot(1, "preparing"));
    __launcherStoreTest.accept(snapshot(2, "starting"));
    expect(__launcherStoreTest.current()?.phase).toBe("starting");
  });
});

describe("launcher selectors", () => {
  it("keeps shallow field selections stable across full snapshot objects", () => {
    expect(
      shallowEqual(
        { language: "zh", marketBusy: false },
        { language: "zh", marketBusy: false },
      ),
    ).toBe(true);
    expect(
      shallowEqual(
        { language: "zh", marketBusy: false },
        { language: "zh", marketBusy: true },
      ),
    ).toBe(false);
  });
});
