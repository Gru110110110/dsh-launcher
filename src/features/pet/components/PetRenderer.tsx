import { useEffect, useRef } from "react";
import type { AnimationConfigWithData, AnimationItem } from "lottie-web";
import type { Language, PetState } from "@/platform/generated/bindings";
import { localized, petAnimation, type PetDefinition } from "../petCatalog";

export function PetRenderer({
  pet,
  state,
  language,
  bubble,
  reducedMotion,
  fallbackBubble,
  className = "",
}: {
  pet: PetDefinition;
  state: PetState;
  language: Language;
  bubble: boolean;
  reducedMotion: boolean;
  fallbackBubble: string;
  className?: string;
}) {
  const host = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const container = host.current;
    if (!container) return;
    let animation: AnimationItem | null = null;
    let cancelled = false;
    void import("lottie-web/build/player/lottie_light").then(
      ({ default: lottie }) => {
        if (cancelled) return;
        const config: AnimationConfigWithData = {
          container,
          renderer: "svg",
          loop: !reducedMotion,
          autoplay: !reducedMotion,
          animationData: petAnimation(pet, state),
          rendererSettings: { progressiveLoad: true },
        };
        animation = lottie.loadAnimation(config);
        if (reducedMotion) animation.goToAndStop(0, true);
      },
    );
    return () => {
      cancelled = true;
      animation?.destroy();
      container.replaceChildren();
    };
  }, [pet, reducedMotion, state]);

  const copy = pet.bubbles?.[state];
  return (
    <div className={`pet-renderer state-${state} ${className}`.trim()}>
      {bubble && (
        <div className="pet-bubble" role="status" aria-live="polite">
          {copy ? localized(copy, language) : fallbackBubble}
        </div>
      )}
      <div
        ref={host}
        className="pet-animation"
        role="img"
        aria-label={`${localized(pet.nickname, language)} · ${state}`}
      />
    </div>
  );
}
