import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { AnimationConfigWithData, AnimationItem } from "lottie-web";
import type { Language, PetState } from "@/platform/generated/bindings";
import { localized, petAnimation, type PetDefinition } from "../petCatalog";
import { PetPlaybackQueue } from "./petPlaybackQueue";

type PetTransitionMode = "immediate" | "cycle-boundary";

export function PetRenderer({
  pet,
  state,
  language,
  bubble,
  reducedMotion,
  fallbackBubble,
  transitionMode = "immediate",
  className = "",
}: {
  pet: PetDefinition;
  state: PetState;
  language: Language;
  bubble: boolean;
  reducedMotion: boolean;
  fallbackBubble: string | ((state: PetState) => string);
  transitionMode?: PetTransitionMode;
  className?: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const [playback] = useState(() => new PetPlaybackQueue(state));
  const [queuedState, setQueuedState] = useState(state);
  const queueEnabled = transitionMode === "cycle-boundary" && !reducedMotion;
  const displayedState = queueEnabled ? queuedState : state;

  useLayoutEffect(() => {
    if (queueEnabled) {
      playback.request(state);
      return;
    }
    playback.reset(state);
    setQueuedState(state);
  }, [playback, queueEnabled, state]);

  useEffect(() => {
    const container = host.current;
    if (!container) return;
    let animation: AnimationItem | null = null;
    let removeCompleteListener: (() => void) | undefined;
    let cancelled = false;
    void import("lottie-web/build/player/lottie_light")
      .then(({ default: lottie }) => {
        if (cancelled) return;
        const config: AnimationConfigWithData = {
          container,
          renderer: "svg",
          loop: !queueEnabled && !reducedMotion,
          autoplay: !queueEnabled && !reducedMotion,
          animationData: petAnimation(pet, displayedState),
          rendererSettings: { progressiveLoad: true },
        };
        animation = lottie.loadAnimation(config);
        if (reducedMotion) {
          animation.goToAndStop(0, true);
        } else if (queueEnabled) {
          removeCompleteListener = animation.addEventListener(
            "complete",
            () => {
              if (cancelled) return;
              const decision = playback.completeCycle();
              if (decision.kind === "switch") {
                setQueuedState(decision.state);
              } else {
                animation?.goToAndPlay(0, true);
              }
            },
          );
          animation.play();
        }
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          console.error("Desktop pet animation failed to load", error);
        }
      });
    return () => {
      cancelled = true;
      removeCompleteListener?.();
      animation?.destroy();
      container.replaceChildren();
    };
  }, [displayedState, pet, playback, queueEnabled, reducedMotion]);

  const copy = pet.bubbles?.[displayedState];
  const defaultCopy =
    typeof fallbackBubble === "function"
      ? fallbackBubble(displayedState)
      : fallbackBubble;
  return (
    <div className={`pet-renderer state-${displayedState} ${className}`.trim()}>
      {bubble && (
        <div className="pet-bubble" role="status" aria-live="polite">
          {copy ? localized(copy, language) : defaultCopy}
        </div>
      )}
      <div
        ref={host}
        className="pet-animation"
        role="img"
        aria-label={`${localized(pet.nickname, language)} · ${displayedState}`}
      />
    </div>
  );
}
