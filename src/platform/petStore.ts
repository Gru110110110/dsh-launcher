import { useSyncExternalStore } from "react";
import type { PetSnapshot } from "./generated/bindings";
import { petApi } from "./petApi";

let current: PetSnapshot | null = null;
let initializationError: Error | null = null;
let initializePromise: Promise<void> | null = null;
const listeners = new Set<() => void>();

function accept(snapshot: PetSnapshot): void {
  if (
    current?.bridgeStatus === "connected" &&
    snapshot.bridgeStatus === "connected" &&
    snapshot.sequence < current.sequence
  )
    return;
  current = snapshot;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function initializePetStore(): Promise<void> {
  if (initializePromise) return initializePromise;
  initializePromise = (async () => {
    try {
      await petApi.onState(accept);
      accept(await petApi.snapshot());
    } catch (error) {
      initializationError =
        error instanceof Error
          ? error
          : new Error("Desktop pet IPC unavailable");
      throw initializationError;
    }
  })();
  return initializePromise;
}

export function initializePetPreview(snapshot: PetSnapshot): void {
  accept(snapshot);
}

export function usePetSnapshot(): PetSnapshot {
  const snapshot = useSyncExternalStore(subscribe, () => current);
  if (initializationError) throw initializationError;
  if (!snapshot) throw initializePetStore();
  return snapshot;
}

export const __petStoreTest = {
  accept,
  current: () => current,
  reset: () => {
    current = null;
    initializationError = null;
    initializePromise = null;
    listeners.clear();
  },
};
