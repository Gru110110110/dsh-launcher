import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type { LauncherSnapshot } from "@/platform/generated/bindings";
import { LauncherPage } from "./LauncherPage";

const state = vi.hoisted(() => ({ snapshot: {} as LauncherSnapshot }));
vi.mock("@/platform/launcherStore", () => ({
  useLauncherSnapshot: () => state.snapshot,
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, values?: { version?: string }) =>
      `${key}${values?.version ?? ""}`,
  }),
}));
vi.mock("./BalanceCard", () => ({ BalanceCard: () => null }));

describe("Harness rollback recovery", () => {
  it.each(["failed", "stopped", "ready", "starting"] as const)(
    "offers the retained runtime only when %s allows rollback",
    (phase) => {
      state.snapshot = {
        phase,
        language: "zh",
        harnessUpdate: { kind: "none" },
        desktopUpdate: { kind: "idle" },
        browsers: [],
        activity: null,
        progress: { kind: "indeterminate" },
        migration: { kind: "notRequired" },
        removedIncompatiblePlugins: [],
        previousHarnessVersion: "0.2.1-rc.2",
        error: null,
      } as unknown as LauncherSnapshot;
      const markup = renderToStaticMarkup(<LauncherPage />);
      expect(markup.includes("action.rollbackHarness0.2.1-rc.2")).toBe(
        phase === "failed" || phase === "stopped",
      );
      state.snapshot.previousHarnessVersion = null;
      expect(renderToStaticMarkup(<LauncherPage />)).not.toContain(
        "action.rollbackHarness",
      );
    },
  );
});
