import { Eye, EyeOff, MousePointer2, PawPrint, Sparkles } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { PetState } from "@/platform/generated/bindings";
import { petApi } from "@/platform/petApi";
import { usePetSnapshot } from "@/platform/petStore";
import { useLauncherSnapshot } from "@/platform/launcherStore";
import { showTimedError } from "@/shared/errorToast";
import { PetRenderer } from "../components/PetRenderer";
import { findPet, localized, petCatalog, petStates } from "../petCatalog";

export function PetPage() {
  const launcher = useLauncherSnapshot();
  const live = usePetSnapshot();
  const { t } = useTranslation(undefined, { lng: launcher.language });
  const [previewState, setPreviewState] = useState<PetState | null>(null);
  const selected = findPet(launcher.pet.selectedPetId);
  const shownState = previewState ?? live.state;
  const run = (task: Promise<unknown>) => {
    void task.catch((error: unknown) => {
      showTimedError(error, (key, values) => t(key, values));
    });
  };

  return (
    <div className="content-page pet-page">
      <header className="page-header pet-page-header">
        <div>
          <h1>
            {t("pet.title")} <Sparkles size={22} aria-hidden />
          </h1>
          <p>{t("pet.subtitle")}</p>
        </div>
        <button
          type="button"
          className={`pet-master-toggle${launcher.pet.enabled ? " enabled" : ""}`}
          aria-pressed={launcher.pet.enabled}
          onClick={() => {
            run(petApi.patchPreferences({ enabled: !launcher.pet.enabled }));
          }}
        >
          {launcher.pet.enabled ? <Eye size={17} /> : <EyeOff size={17} />}
          {launcher.pet.enabled ? t("pet.visible") : t("pet.hidden")}
        </button>
      </header>

      <section className="page-section pet-picker-section">
        <h2 className="section-label">{t("pet.choose")}</h2>
        <div className="pet-picker-grid">
          {petCatalog.pets.map((pet) => {
            const active = pet.id === selected.id;
            return (
              <button
                type="button"
                className={`pet-choice${active ? " selected" : ""}`}
                aria-pressed={active}
                key={pet.id}
                onClick={() => {
                  run(petApi.patchPreferences({ selectedPetId: pet.id }));
                }}
              >
                <PetRenderer
                  pet={pet}
                  state="idle"
                  language={launcher.language}
                  bubble={false}
                  reducedMotion
                  fallbackBubble=""
                  className="pet-choice-renderer"
                />
                <span>
                  <strong>{localized(pet.nickname, launcher.language)}</strong>
                  <small>{localized(pet.name, launcher.language)}</small>
                </span>
              </button>
            );
          })}
        </div>
      </section>

      <div className="pet-dashboard-grid">
        <section className="panel pet-preview-panel">
          <div className="pet-preview-status">
            <i className={`pet-state-dot state-${shownState}`} />
            {t("pet.currentState", { state: t(`pet.state.${shownState}`) })}
          </div>
          <PetRenderer
            pet={selected}
            state={shownState}
            language={launcher.language}
            bubble={launcher.pet.bubbleEnabled}
            reducedMotion={launcher.pet.reducedMotion}
            fallbackBubble={t(`pet.defaultBubble.${shownState}`)}
            className="pet-large-preview"
          />
          <div
            className="pet-state-picker"
            role="radiogroup"
            aria-label={t("pet.previewState")}
          >
            {petStates.map((state) => (
              <button
                type="button"
                role="radio"
                aria-checked={shownState === state}
                className={shownState === state ? "selected" : ""}
                key={state}
                onClick={() => {
                  setPreviewState(state);
                }}
              >
                <i className={`pet-state-dot state-${state}`} />
                {t(`pet.state.${state}`)}
              </button>
            ))}
          </div>
        </section>

        <aside className="panel pet-detail-panel">
          <div className="pet-identity">
            <PawPrint size={24} aria-hidden />
            <div>
              <strong>{localized(selected.nickname, launcher.language)}</strong>
              <span>{localized(selected.name, launcher.language)}</span>
            </div>
          </div>
          <div className="pet-tags">
            {selected.tags.map((tag) => (
              <span key={tag.zh}>{localized(tag, launcher.language)}</span>
            ))}
          </div>
          <p>{localized(selected.description, launcher.language)}</p>

          <div className="pet-settings">
            <label>
              <span>{t("pet.size")}</span>
              <input
                type="range"
                min="0.65"
                max="1.4"
                step="0.05"
                value={launcher.pet.scale}
                onChange={(event) => {
                  run(
                    petApi.patchPreferences({
                      scale: Number(event.currentTarget.value),
                    }),
                  );
                }}
              />
            </label>
            <label className="pet-checkbox">
              <input
                type="checkbox"
                checked={launcher.pet.bubbleEnabled}
                onChange={(event) => {
                  run(
                    petApi.patchPreferences({
                      bubbleEnabled: event.currentTarget.checked,
                    }),
                  );
                }}
              />
              <span>{t("pet.showBubble")}</span>
            </label>
            <label className="pet-checkbox">
              <input
                type="checkbox"
                checked={launcher.pet.clickThrough}
                onChange={(event) => {
                  run(
                    petApi.patchPreferences({
                      clickThrough: event.currentTarget.checked,
                    }),
                  );
                }}
              />
              <span>
                <MousePointer2 size={15} />
                {t("pet.clickThrough")}
              </span>
            </label>
            <label className="pet-checkbox">
              <input
                type="checkbox"
                checked={launcher.pet.reducedMotion}
                onChange={(event) => {
                  run(
                    petApi.patchPreferences({
                      reducedMotion: event.currentTarget.checked,
                    }),
                  );
                }}
              />
              <span>{t("pet.reducedMotion")}</span>
            </label>
          </div>
        </aside>
      </div>
    </div>
  );
}
