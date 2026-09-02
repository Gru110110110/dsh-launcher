import { afterEach, describe, expect, it } from "vitest";
import type { PetBridgeStatus, PetSnapshot } from "./generated/bindings";
import { __petStoreTest } from "./petStore";

function snapshot(
  sequence: number,
  bridgeStatus: PetBridgeStatus,
): PetSnapshot {
  return {
    bridgeStatus,
    state: "idle",
    phase: "test",
    activity: null,
    toolName: null,
    project: null,
    task: null,
    progress: null,
    sequence,
    updatedAtMs: 1,
  };
}

describe("desktop pet state stream", () => {
  afterEach(() => {
    __petStoreTest.reset();
  });

  it("rejects out-of-order events from one bridge", () => {
    __petStoreTest.accept(snapshot(9, "connected"));
    __petStoreTest.accept(snapshot(8, "connected"));
    expect(__petStoreTest.current()?.sequence).toBe(9);
  });

  it("accepts a reset before a replacement bridge starts at sequence zero", () => {
    __petStoreTest.accept(snapshot(9, "connected"));
    __petStoreTest.accept(snapshot(0, "unavailable"));
    __petStoreTest.accept(snapshot(0, "connected"));
    expect(__petStoreTest.current()?.bridgeStatus).toBe("connected");
    expect(__petStoreTest.current()?.sequence).toBe(0);
  });
});
