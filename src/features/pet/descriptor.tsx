import { lazy } from "react";
import { PawPrint } from "lucide-react";
import type { FeatureDescriptor } from "@/app/feature";

const PetPage = lazy(async () => {
  const module = await import("./pages/PetPage");
  return { default: module.PetPage };
});

export const petFeature: FeatureDescriptor = {
  id: "pet",
  routes: [{ path: "pet", element: <PetPage /> }],
  navigation: {
    labelKey: "nav.pet",
    path: "/pet",
    icon: PawPrint,
    order: 20,
  },
};
