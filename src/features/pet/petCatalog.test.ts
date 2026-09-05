import { describe, expect, it } from "vitest";
import {
  findPet,
  localized,
  petAnimation,
  petCatalog,
  petStates,
} from "./petCatalog";

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

  it("includes Juzi the orange cat without replacing the default marmot", () => {
    const pet = findPet("orange-cat");
    expect(pet.id).toBe("orange-cat");
    expect(localized(pet.nickname, "zh")).toBe("橘子");
    expect(localized(pet.nickname, "en")).toBe("Juzi");
    expect(localized(pet.name, "zh")).toBe("橘猫");
    expect(petCatalog.pets[0]?.id).toBe("marmot");
  });

  it("resolves every state's image layers without mutating shared animations", () => {
    for (const pet of petCatalog.pets) {
      for (const state of petStates) {
        const data = petAnimation(pet, state);
        expect(data.w).toBe(512);
        expect(data.h).toBe(512);
        expect(data.layers).toBeInstanceOf(Array);
        expect(data.assets?.length).toBeGreaterThan(0);
        for (const asset of data.assets ?? []) {
          if ("layers" in asset) {
            expect(asset.layers).toBeInstanceOf(Array);
            expect(asset.p).toBeUndefined();
            continue;
          }
          expect(asset.u).toBe("");
          expect(asset.p).toEqual(expect.any(String));
          expect(asset.p).not.toBe("");
          expect(asset.e).toBe(0);
        }
        const fresh = petAnimation(pet, state);
        expect(fresh).toEqual(data);
        expect(fresh).not.toBe(data);
        expect(fresh.assets).not.toBe(data.assets);
      }
    }
  });
});
