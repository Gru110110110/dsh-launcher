import rawCatalog from "../../../pets/config.json";
import type { Language, PetState } from "@/platform/generated/bindings";

interface LocalizedText {
  zh: string;
  en: string;
}

export interface PetDefinition {
  id: string;
  nickname: LocalizedText;
  name: LocalizedText;
  folder: string;
  tags: LocalizedText[];
  description: LocalizedText;
  animations: Record<PetState, string>;
  bubbles?: Partial<Record<PetState, LocalizedText>>;
}

interface PetCatalog {
  version: number;
  count: number;
  pets: PetDefinition[];
}

type LottieData = Record<string, unknown> & {
  assets?: Array<
    Record<string, unknown> & { u?: string; p?: string; e?: number }
  >;
};

const animationFiles = import.meta.glob<LottieData>("../../../pets/*/*.json", {
  eager: true,
  import: "default",
});
const imageFiles = import.meta.glob<string>("../../../pets/*/images/**/*.png", {
  eager: true,
  query: "?url",
  import: "default",
});

function validSegment(value: string): boolean {
  return /^[a-z0-9_-]+$/.test(value);
}

function validateCatalog(value: PetCatalog): PetCatalog {
  if (value.version !== 1 || value.count !== value.pets.length) {
    throw new Error("Invalid desktop pet catalog metadata");
  }
  const seen = new Set<string>();
  for (const pet of value.pets) {
    if (
      !validSegment(pet.id) ||
      !validSegment(pet.folder) ||
      seen.has(pet.id)
    ) {
      throw new Error(`Invalid desktop pet id: ${pet.id}`);
    }
    seen.add(pet.id);
    for (const state of petStates) {
      const file = pet.animations[state];
      if (!/^[a-z0-9_-]+\.json$/.test(file)) {
        throw new Error(`Invalid ${state} animation for ${pet.id}`);
      }
      if (!animationFiles[`../../../pets/${pet.folder}/${file}`]) {
        throw new Error(`Missing ${state} animation for ${pet.id}`);
      }
    }
  }
  return value;
}

export const petStates = [
  "waiting",
  "error",
  "working",
  "thinking",
  "idle",
] as const satisfies readonly PetState[];

export const petCatalog = validateCatalog(rawCatalog as PetCatalog);

export function localized(text: LocalizedText, language: Language): string {
  return text[language];
}

export function findPet(id: string): PetDefinition {
  const pet =
    petCatalog.pets.find((entry) => entry.id === id) ?? petCatalog.pets[0];
  if (!pet) throw new Error("The desktop pet catalog is empty");
  return pet;
}

export function petAnimation(pet: PetDefinition, state: PetState): LottieData {
  const source =
    animationFiles[`../../../pets/${pet.folder}/${pet.animations[state]}`];
  if (!source) throw new Error(`Missing ${state} animation for ${pet.id}`);
  const data = structuredClone(source);
  for (const asset of data.assets ?? []) {
    if (typeof asset.u !== "string" || typeof asset.p !== "string") continue;
    const original = `../../../pets/${pet.folder}/${asset.u}${asset.p}`;
    const url = imageFiles[original];
    if (!url) throw new Error(`Missing desktop pet image: ${original}`);
    asset.u = "";
    asset.p = url;
    asset.e = 0;
  }
  return data;
}
