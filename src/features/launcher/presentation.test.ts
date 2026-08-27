import { describe, expect, it } from "vitest";
import type { LauncherSnapshot } from "@/platform/generated/bindings";
import { getHarnessUpdateNotice, getServiceCopy } from "./presentation";

function snapshot(overrides: Partial<LauncherSnapshot> = {}): LauncherSnapshot {
  return {
    revision: 1,
    marketBusy: false,
    marketRevision: 0,
    marketCatalogRevision: 0,
    phase: "preparing",
    step: "prepare",
    activity: null,
    progress: { kind: "indeterminate" },
    error: null,
    webUrl: null,
    serviceStartedAtMs: null,
    browsers: [],
    selectedBrowserId: "system",
    language: "zh",
    theme: "system",
    desktopVersion: "0.2.0",
    harnessVersion: null,
    desktopUpdate: { kind: "idle" },
    harnessUpdate: { kind: "none" },
    migration: { kind: "notRequired" },
    trayAvailable: true,
    showBalanceCard: true,
    ...overrides,
  };
}

describe("launcher presentation", () => {
  it("uses update service copy while a Harness update is installing", () => {
    const value = snapshot({
      harnessVersion: "0.1.0-rc.6",
      harnessUpdate: { kind: "installing", version: "0.1.0-rc.7" },
    });

    expect(getServiceCopy(value)).toEqual({
      title: "service.updateTitle",
      badge: "service.updating",
      busyAction: "action.updating",
    });
    expect(getHarnessUpdateNotice(value)).toBeNull();
  });

  it("shows only actionable Harness update notices", () => {
    expect(
      getHarnessUpdateNotice(
        snapshot({
          harnessUpdate: { kind: "available", version: "0.1.0-rc.7" },
        }),
      ),
    ).toEqual({
      message: {
        key: "update.harness.available",
        values: { version: "0.1.0-rc.7" },
      },
      tone: "info",
      actionLabel: "action.updateHarness",
    });
    expect(
      getHarnessUpdateNotice(snapshot({ harnessUpdate: { kind: "checking" } })),
    ).toBeNull();
  });

  it("offers a retry after a Harness update failure", () => {
    expect(
      getHarnessUpdateNotice(
        snapshot({
          harnessUpdate: { kind: "failed", version: "0.1.0-rc.7" },
        }),
      ),
    ).toMatchObject({
      tone: "error",
      actionLabel: "action.retryUpdate",
    });
  });

  it("offers activation after a background update is downloaded", () => {
    expect(
      getHarnessUpdateNotice(
        snapshot({
          harnessUpdate: { kind: "downloaded", version: "0.1.0-rc.7" },
        }),
      ),
    ).toEqual({
      message: {
        key: "update.harness.downloaded",
        values: { version: "0.1.0-rc.7" },
      },
      tone: "info",
      actionLabel: "action.restartAndUpdate",
    });
    expect(
      getHarnessUpdateNotice(
        snapshot({
          harnessUpdate: { kind: "downloading", version: "0.1.0-rc.7" },
        }),
      ),
    ).toBeNull();
  });

  it("labels stopping independently from preparing or starting", () => {
    expect(
      getServiceCopy(snapshot({ phase: "stopping", step: "start" })),
    ).toEqual({
      title: "service.title",
      badge: "service.stopping",
      busyAction: "action.stopping",
    });
  });
});
