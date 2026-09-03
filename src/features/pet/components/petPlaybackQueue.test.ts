import { describe, expect, it } from "vitest";
import { PetPlaybackQueue } from "./petPlaybackQueue";

describe("desktop pet playback queue", () => {
  it("waits for the current animation cycle before switching", () => {
    const queue = new PetPlaybackQueue("working");
    queue.request("thinking");

    expect(queue.displayedState).toBe("working");
    expect(queue.completeCycle()).toEqual({
      kind: "switch",
      state: "thinking",
    });
    expect(queue.displayedState).toBe("thinking");
  });

  it("coalesces repeated states into one pending transition", () => {
    const queue = new PetPlaybackQueue("working");
    queue.request("thinking");
    queue.request("thinking");
    queue.request("thinking");

    expect(queue.completeCycle()).toEqual({
      kind: "switch",
      state: "thinking",
    });
    expect(queue.completeCycle()).toEqual({
      kind: "repeat",
      state: "thinking",
    });
  });

  it("keeps only the latest state and drops obsolete intermediate states", () => {
    const queue = new PetPlaybackQueue("working");
    queue.request("thinking");
    queue.request("waiting");

    expect(queue.completeCycle()).toEqual({
      kind: "switch",
      state: "waiting",
    });
  });

  it("continues looping when the latest state matches the current state", () => {
    const queue = new PetPlaybackQueue("working");
    queue.request("thinking");
    queue.request("working");

    expect(queue.completeCycle()).toEqual({
      kind: "repeat",
      state: "working",
    });
  });

  it("resets immediately for non-animated presentation modes", () => {
    const queue = new PetPlaybackQueue("working");
    queue.request("thinking");
    queue.reset("idle");

    expect(queue.completeCycle()).toEqual({
      kind: "repeat",
      state: "idle",
    });
  });
});
