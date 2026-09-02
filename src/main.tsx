import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@/i18n";
import "./styles.css";
import { AppBootstrap } from "@/app/AppBootstrap";
import {
  initializeLauncherPreview,
  initializeLauncherStore,
} from "@/platform/launcherStore";
import { startRemoteLanMonitorWhenReady } from "@/platform/remoteLanMonitor";
import { initializePetPreview, initializePetStore } from "@/platform/petStore";
import type { LauncherSnapshot } from "@/platform/generated/bindings";
import packageJson from "../package.json";

const isTauri = "__TAURI_INTERNALS__" in window;
if (import.meta.env.DEV && !isTauri) {
  const previewSnapshot: LauncherSnapshot = {
    revision: 1,
    marketBusy: false,
    marketRevision: 0,
    marketCatalogRevision: 0,
    phase: "ready",
    step: "start",
    activity: null,
    progress: { kind: "indeterminate" },
    error: null,
    webUrl: "http://127.0.0.1:3080",
    serviceStartedAtMs: Date.now() - 51_680_000,
    browsers: [
      { id: "system", label: "Default browser" },
      { id: "chrome", label: "Google Chrome" },
    ],
    selectedBrowserId: "chrome",
    language: "zh",
    theme: "system",
    desktopVersion: packageJson.version,
    harnessVersion: "0.1.0-rc.7",
    previousHarnessVersion: "0.1.0-rc.6",
    removedIncompatiblePlugins: [],
    repairedProjectionCache: false,
    desktopUpdate: { kind: "idle" },
    harnessUpdate: { kind: "none" },
    migration: { kind: "notRequired" },
    trayAvailable: true,
    showBalanceCard: true,
    harnessUpdateChannel: "latest",
    proxy: { mode: "system", url: "", bypass: "" },
    remote: {
      master: false,
      serviceReady: true,
      lan: { enabled: false, available: false, url: null, password: "" },
      public: {
        enabled: false,
        state: "off",
        url: null,
        password: "",
        error: null,
      },
    },
    pet: {
      enabled: true,
      selectedPetId: "marmot",
      scale: 1,
      bubbleEnabled: true,
      clickThrough: false,
      reducedMotion: false,
      position: null,
    },
  };
  initializeLauncherPreview(previewSnapshot);
  initializePetPreview({
    bridgeStatus: "connected",
    state: "idle",
    phase: "preview",
    activity: null,
    toolName: null,
    project: null,
    task: null,
    progress: null,
    sequence: 1,
    updatedAtMs: Date.now(),
  });
} else {
  startRemoteLanMonitorWhenReady(initializeLauncherStore());
  void initializePetStore();
}

const root = document.getElementById("root");
if (!root) throw new Error("Missing root element");
createRoot(root).render(
  <StrictMode>
    <AppBootstrap />
  </StrictMode>,
);
