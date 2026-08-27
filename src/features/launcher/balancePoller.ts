import type { BalanceSnapshot } from "@/platform/generated/bindings";

const BALANCE_POLL_INTERVAL_MS = 5 * 60 * 1000;

type BalancePollerOptions = {
  fetch: () => Promise<BalanceSnapshot>;
  onUpdate: (snapshot: BalanceSnapshot) => void;
  onError?: (error: unknown) => void;
  intervalMs?: number;
};

export function createBalancePoller(options: BalancePollerOptions) {
  const intervalMs = options.intervalMs ?? BALANCE_POLL_INTERVAL_MS;
  let timer: ReturnType<typeof setInterval> | null = null;
  let inFlight = false;
  let running = false;
  const isRunning = () => running;

  const clearTimer = () => {
    if (timer !== null) clearInterval(timer);
    timer = null;
  };
  const tick = async () => {
    if (!isRunning() || inFlight) return;
    inFlight = true;
    try {
      const snapshot = await options.fetch();
      if (isRunning()) options.onUpdate(snapshot);
    } catch (error) {
      if (isRunning()) options.onError?.(error);
    } finally {
      inFlight = false;
    }
  };
  const schedule = () => {
    clearTimer();
    if (!isRunning()) return;
    timer = setInterval(() => void tick(), intervalMs);
  };

  return {
    start() {
      if (running) return;
      running = true;
      void tick();
      schedule();
    },
    stop() {
      running = false;
      clearTimer();
    },
    resetSchedule() {
      schedule();
    },
  };
}
