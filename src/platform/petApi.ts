import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { PetPreferencesPatch, PetSnapshot } from "./generated/bindings";

const emptyPatch = (): PetPreferencesPatch => ({
  enabled: null,
  selectedPetId: null,
  scale: null,
  bubbleEnabled: null,
  clickThrough: null,
  reducedMotion: null,
  position: null,
});

export const petApi = {
  snapshot: () => invoke<PetSnapshot>("pet_get_snapshot"),
  patchPreferences: (patch: Partial<PetPreferencesPatch>) =>
    invoke("preferences_patch_pet", {
      patch: { ...emptyPatch(), ...patch },
    }),
  onState: (handler: (snapshot: PetSnapshot) => void): Promise<UnlistenFn> =>
    listen<PetSnapshot>("pet://state", ({ payload }) => {
      handler(payload);
    }),
};
