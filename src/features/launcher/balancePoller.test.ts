import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { BalanceSnapshot } from "@/platform/generated/bindings";
import { createBalancePoller } from "./balancePoller";

const snapshot: BalanceSnapshot = {
  status: "ok",
  detail: null,
  isAvailable: true,
  currency: "CNY",
  totalBalance: "1.00",
  fetchedAtMs: 1,
};

describe("balance poller", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("fetches immediately and then on the interval", async () => {
    const fetch = vi.fn(() => Promise.resolve(snapshot));
    const onUpdate = vi.fn();
    const poller = createBalancePoller({ fetch, onUpdate, intervalMs: 1000 });
    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(onUpdate).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(2000);
    expect(fetch).toHaveBeenCalledTimes(3);
    poller.stop();
  });

  it("stops completely and drops an in-flight result", async () => {
    let resolve: ((value: BalanceSnapshot) => void) | undefined;
    const onUpdate = vi.fn();
    const poller = createBalancePoller({
      fetch: () =>
        new Promise((done) => {
          resolve = done;
        }),
      onUpdate,
      intervalMs: 1000,
    });
    poller.start();
    poller.stop();
    resolve?.(snapshot);
    await vi.advanceTimersByTimeAsync(2000);
    expect(onUpdate).not.toHaveBeenCalled();
  });

  it("restarts the interval after manual refresh", async () => {
    const fetch = vi.fn(() => Promise.resolve(snapshot));
    const poller = createBalancePoller({
      fetch,
      onUpdate: vi.fn(),
      intervalMs: 1000,
    });
    poller.start();
    await vi.advanceTimersByTimeAsync(600);
    poller.resetSchedule();
    await vi.advanceTimersByTimeAsync(500);
    expect(fetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(500);
    expect(fetch).toHaveBeenCalledTimes(2);
    poller.stop();
  });
});
