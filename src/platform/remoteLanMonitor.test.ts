import { afterEach, describe, expect, it, vi } from "vitest";
import { startRemoteLanMonitor } from "./remoteLanMonitor";

describe("remote LAN monitor", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("refreshes immediately, periodically, and when the window regains focus", async () => {
    vi.useFakeTimers();
    const windowEvents = new EventTarget();
    const documentEvents = new EventTarget();
    Object.defineProperty(documentEvents, "visibilityState", {
      configurable: true,
      value: "visible",
    });
    vi.stubGlobal(
      "window",
      Object.assign(windowEvents, {
        clearInterval: globalThis.clearInterval,
        setInterval: globalThis.setInterval,
      }),
    );
    vi.stubGlobal("document", documentEvents);
    const refresh = vi.fn(async () => {});

    const stop = startRemoteLanMonitor(refresh);
    await vi.advanceTimersByTimeAsync(0);
    expect(refresh).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(10_000);
    expect(refresh).toHaveBeenCalledTimes(2);

    windowEvents.dispatchEvent(new Event("focus"));
    await vi.advanceTimersByTimeAsync(0);
    expect(refresh).toHaveBeenCalledTimes(3);

    stop();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(refresh).toHaveBeenCalledTimes(3);
  });
});
