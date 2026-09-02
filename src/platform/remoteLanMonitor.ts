import { launcherApi } from "./launcherApi";

const REFRESH_INTERVAL_MS = 10_000;

/** Keeps LAN availability and its wildcard listener aligned with route changes. */
export function startRemoteLanMonitor(
  refreshLan: () => Promise<void> = launcherApi.remoteRefreshLan,
): () => void {
  let stopped = false;
  let inFlight = false;
  let errorReported = false;
  const refresh = () => {
    if (stopped || inFlight || document.visibilityState === "hidden") return;
    inFlight = true;
    void refreshLan()
      .then(() => {
        errorReported = false;
      })
      .catch((error: unknown) => {
        if (!stopped && !errorReported) {
          errorReported = true;
          console.warn("LAN availability refresh failed", error);
        }
      })
      .finally(() => {
        inFlight = false;
      });
  };
  const refreshWhenVisible = () => {
    if (document.visibilityState === "visible") refresh();
  };

  refresh();
  const interval = window.setInterval(refresh, REFRESH_INTERVAL_MS);
  window.addEventListener("focus", refresh);
  document.addEventListener("visibilitychange", refreshWhenVisible);
  return () => {
    stopped = true;
    window.clearInterval(interval);
    window.removeEventListener("focus", refresh);
    document.removeEventListener("visibilitychange", refreshWhenVisible);
  };
}

/** Starts LAN monitoring only after IPC bootstrap and consumes bootstrap errors. */
export function startRemoteLanMonitorWhenReady(
  initialization: Promise<void>,
  startMonitor: () => void = () => {
    startRemoteLanMonitor();
  },
  reportError: (error: unknown) => void = (error) => {
    // The launcher store retains this error and the React error boundary shows
    // the fatal UI. Consume this branch here as well so the eager bootstrap in
    // main.tsx cannot create a global unhandled Promise rejection.
    console.error("Launcher IPC initialization failed", error);
  },
): void {
  void initialization.then(startMonitor).catch(reportError);
}
