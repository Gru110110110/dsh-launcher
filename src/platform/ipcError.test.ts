import { describe, expect, it } from "vitest";
import { asIpcError } from "./ipcError";

describe("IPC errors", () => {
  it("preserves errors returned as strings by Tauri", () => {
    expect(
      asIpcError("state not managed for launcher_get_snapshot").message,
    ).toBe("state not managed for launcher_get_snapshot");
  });

  it("preserves native Error instances", () => {
    const error = new Error("native failure");
    expect(asIpcError(error)).toBe(error);
  });
});
