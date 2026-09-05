import { EyeOff } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { petApi } from "@/platform/petApi";
import { usePetSnapshot } from "@/platform/petStore";
import { useLauncherSnapshot } from "@/platform/launcherStore";
import { PetRenderer } from "../components/PetRenderer";
import { findPet } from "../petCatalog";
import { enqueuePetWindowSync, syncPetWindow } from "./petWindowSync";

export function PetWindow() {
  const launcher = useLauncherSnapshot();
  const live = usePetSnapshot();
  const { t } = useTranslation(undefined, { lng: launcher.language });
  const saveTimer = useRef<number | null>(null);
  const syncQueue = useRef(Promise.resolve());
  const initialPosition = useRef(launcher.pet.position);
  const positioned = useRef(false);
  const [contextMenu, setContextMenu] = useState<{
    left: number;
    top: number;
  } | null>(null);
  const selected = findPet(launcher.pet.selectedPetId);
  const active = launcher.pet.enabled && launcher.phase === "ready";

  useEffect(() => {
    document.documentElement.classList.add("pet-window-root");
    return () => {
      document.documentElement.classList.remove("pet-window-root");
    };
  }, []);

  useEffect(() => {
    if (!contextMenu) return;
    const dismiss = () => {
      setContextMenu(null);
    };
    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") dismiss();
    };
    window.addEventListener("blur", dismiss);
    window.addEventListener("keydown", dismissOnEscape);
    return () => {
      window.removeEventListener("blur", dismiss);
      window.removeEventListener("keydown", dismissOnEscape);
    };
  }, [contextMenu]);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let cancelled = false;
    const isCancelled = () => cancelled;
    syncQueue.current = enqueuePetWindowSync(
      syncQueue.current,
      async () => {
        if (isCancelled()) return;
        const api = await import("@tauri-apps/api/window");
        if (isCancelled()) return;
        const current = api.getCurrentWindow();
        const positionApplied = await syncPetWindow(
          {
            setSize: (width, height) =>
              current.setSize(new api.LogicalSize(width, height)),
            setClickThrough: (enabled) =>
              current.setIgnoreCursorEvents(enabled),
            hide: () => current.hide(),
            availableMonitors: api.availableMonitors,
            outerSize: () => current.outerSize(),
            outerPosition: () => current.outerPosition(),
            primaryMonitor: api.primaryMonitor,
            setPosition: (x, y) =>
              current.setPosition(new api.PhysicalPosition(x, y)),
            show: () => current.show(),
          },
          {
            active,
            clickThrough: launcher.pet.clickThrough,
            scale: launcher.pet.scale,
            initialPosition: initialPosition.current,
            positioned: positioned.current,
          },
          isCancelled,
        );
        if (positionApplied) {
          positioned.current = true;
        }
      },
      (error) => {
        console.error("Desktop pet window synchronization failed", error);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [active, launcher.pet.clickThrough, launcher.pet.scale]);

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/window").then(async ({ getCurrentWindow }) => {
      if (cancelled) return;
      unlisten = await getCurrentWindow().onMoved(({ payload }) => {
        if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
        saveTimer.current = window.setTimeout(() => {
          void petApi.patchPreferences({
            position: { x: payload.x, y: payload.y },
          });
        }, 250);
      });
    });
    return () => {
      cancelled = true;
      unlisten?.();
      if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    };
  }, []);

  return (
    <main
      className="desktop-pet"
      onPointerDown={(event) => {
        if ((event.target as Element).closest(".pet-context-menu")) return;
        setContextMenu(null);
        if (
          event.button !== 0 ||
          launcher.pet.clickThrough ||
          !("__TAURI_INTERNALS__" in window)
        )
          return;
        void import("@tauri-apps/api/window").then(({ getCurrentWindow }) =>
          getCurrentWindow().startDragging(),
        );
      }}
      onContextMenu={(event) => {
        event.preventDefault();
        if (launcher.pet.clickThrough) return;
        const margin = 8;
        const menuWidth = 136;
        const menuHeight = 42;
        setContextMenu({
          left: Math.max(
            margin,
            Math.min(event.clientX, window.innerWidth - menuWidth - margin),
          ),
          top: Math.max(
            margin,
            Math.min(event.clientY, window.innerHeight - menuHeight - margin),
          ),
        });
      }}
    >
      <PetRenderer
        pet={selected}
        state={live.state}
        language={launcher.language}
        bubble={launcher.pet.bubbleEnabled}
        reducedMotion={launcher.pet.reducedMotion}
        fallbackBubble={(state) => t(`pet.defaultBubble.${state}`)}
        transitionMode="cycle-boundary"
        className="pet-window-renderer"
      />
      {contextMenu && (
        <div
          className="pet-context-menu"
          role="menu"
          style={contextMenu}
          onPointerDown={(event) => {
            event.stopPropagation();
          }}
        >
          <button
            type="button"
            role="menuitem"
            autoFocus
            onClick={() => {
              setContextMenu(null);
              void petApi
                .patchPreferences({ enabled: false })
                .catch((error: unknown) => {
                  console.error("Desktop pet could not be hidden", error);
                });
            }}
          >
            <EyeOff size={15} aria-hidden />
            {t("pet.hideAction")}
          </button>
        </div>
      )}
    </main>
  );
}
