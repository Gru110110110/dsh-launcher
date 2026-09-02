import { describe, expect, it } from "vitest";
import { findPet, localized, petCatalog, petStates } from "./petCatalog";

describe("desktop pet catalog", () => {
  it("matches the declared count and provides all five animations", () => {
    expect(petCatalog.pets).toHaveLength(petCatalog.count);
    for (const pet of petCatalog.pets) {
      expect(Object.keys(pet.animations).sort()).toEqual([...petStates].sort());
    }
  });

  it("provides bilingual metadata and falls back to the first pet", () => {
    const pet = findPet("missing-pet");
    expect(localized(pet.nickname, "zh")).toBeTruthy();
    expect(localized(pet.nickname, "en")).toBeTruthy();
    expect(pet).toBe(petCatalog.pets[0]);
  });
});
