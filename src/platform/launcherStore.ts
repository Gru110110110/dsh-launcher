import { useRef, useSyncExternalStore } from "react";
import type { LauncherSnapshot } from "./generated/bindings";
import { asIpcError } from "./ipcError";
import { launcherApi } from "./launcherApi";

let current: LauncherSnapshot | null = null;
let initializationError: Error | null = null;
const listeners = new Set<() => void>();
let initializePromise: Promise<void> | null = null;

function accept(snapshot: LauncherSnapshot): void {
  if (current && snapshot.revision < current.revision) return;
  current = snapshot;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function initializeLauncherStore(): Promise<void> {
  if (initializePromise) return initializePromise;
  initializePromise = (async () => {
    try {
      await launcherApi.onState(accept);
      accept(await launcherApi.snapshot());
    } catch (error) {
      initializationError = asIpcError(error);
      throw initializationError;
    }
  })();
  return initializePromise;
}

export function shallowEqual<T>(left: T, right: T): boolean {
  if (Object.is(left, right)) return true;
  if (
    typeof left !== "object" ||
    left === null ||
    typeof right !== "object" ||
    right === null
  ) {
    return false;
  }
  const leftRecord = left as Record<string, unknown>;
  const rightRecord = right as Record<string, unknown>;
  const keys = Object.keys(leftRecord);
  if (keys.length !== Object.keys(rightRecord).length) return false;
  return keys.every(
    (key) =>
      Object.hasOwn(rightRecord, key) &&
      Object.is(leftRecord[key], rightRecord[key]),
  );
}

export function useLauncherSelector<T>(
  selector: (snapshot: LauncherSnapshot) => T,
  isEqual: (left: T, right: T) => boolean = Object.is,
): T {
  const cache = useRef<{ value: T } | null>(null);
  const selection = useSyncExternalStore(subscribe, () => {
    if (!current) return null;
    const value = selector(current);
    if (cache.current && isEqual(cache.current.value, value)) {
      return cache.current;
    }
    cache.current = { value };
    return cache.current;
  });
  if (initializationError) throw initializationError;
  if (!selection) throw initializeLauncherStore();
  return selection.value;
}

export function useLauncherSnapshot(): LauncherSnapshot {
  return useLauncherSelector((snapshot) => snapshot);
}

export const __launcherStoreTest = {
  accept,
  reset: () => {
    current = null;
    initializationError = null;
    initializePromise = null;
    listeners.clear();
  },
  current: () => current,
};

export function initializeLauncherPreview(snapshot: LauncherSnapshot): void {
  accept(snapshot);
}
