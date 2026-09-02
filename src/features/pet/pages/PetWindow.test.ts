import { describe, expect, it, vi } from "vitest";
import {
  enqueuePetWindowSync,
  syncPetWindow,
  type PetWindowOperations,
} from "./petWindowSync";

function operations(
  overrides: Partial<PetWindowOperations> = {},
): PetWindowOperations {
  return {
    setSize: vi.fn().mockResolvedValue(undefined),
    setClickThrough: vi.fn().mockResolvedValue(undefined),
    hide: vi.fn().mockResolvedValue(undefined),
    availableMonitors: vi.fn().mockResolvedValue([]),
    outerSize: vi.fn().mockResolvedValue({ width: 260, height: 310 }),
    outerPosition: vi.fn().mockResolvedValue({ x: 10, y: 20 }),
    primaryMonitor: vi.fn().mockResolvedValue(null),
    setPosition: vi.fn().mockResolvedValue(undefined),
    show: vi.fn().mockResolvedValue(undefined),
    ...overrides,
  };
}

const activeOptions = {
  active: true,
  clickThrough: false,
  scale: 1,
  initialPosition: null,
  positioned: false,
};

describe("desktop pet window synchronization", () => {
  it("serializes effects so the newest visibility state lands last", async () => {
    let releaseFirst = () => {};
    const firstSettled = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    const calls: string[] = [];
    const reportError = vi.fn();
    const first = enqueuePetWindowSync(
      Promise.resolve(),
      async () => {
        calls.push("old-show-start");
        await firstSettled;
        calls.push("old-show-end");
      },
      reportError,
    );
    const second = enqueuePetWindowSync(
      first,
      () => {
        calls.push("new-hide");
        return Promise.resolve();
      },
      reportError,
    );

    await Promise.resolve();
    expect(calls).toEqual(["old-show-start"]);
    releaseFirst();
    await second;

    expect(calls).toEqual(["old-show-start", "old-show-end", "new-hide"]);
    expect(reportError).not.toHaveBeenCalled();
  });

  it("stops an obsolete effect after an awaited window operation", async () => {
    let cancel = false;
    let resolveSize = () => {};
    const sizeSettled = new Promise<void>((resolve) => {
      resolveSize = resolve;
    });
    const calls: string[] = [];
    const api = operations({
      setSize: async () => {
        calls.push("size");
        await sizeSettled;
      },
      setClickThrough: () => {
        calls.push("click-through");
        return Promise.resolve();
      },
    });

    const synchronization = syncPetWindow(api, activeOptions, () => cancel);
    cancel = true;
    resolveSize();

    await expect(synchronization).resolves.toBe(false);
    expect(calls).toEqual(["size"]);
    expect(api.show).not.toHaveBeenCalled();
  });

  it("does not show the window when cancellation lands after positioning", async () => {
    let cancel = false;
    const setPosition = vi.fn(() => {
      cancel = true;
      return Promise.resolve();
    });
    const api = operations({
      availableMonitors: vi.fn().mockResolvedValue([
        {
          workArea: {
            position: { x: 0, y: 0 },
            size: { width: 1920, height: 1080 },
          },
        },
      ]),
      setPosition,
    });

    await expect(syncPetWindow(api, activeOptions, () => cancel)).resolves.toBe(
      false,
    );
    expect(setPosition).toHaveBeenCalledOnce();
    expect(api.show).not.toHaveBeenCalled();
  });
});
