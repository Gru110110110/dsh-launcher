import { lazy } from "react";
import { Puzzle } from "lucide-react";
import type { FeatureDescriptor } from "@/app/feature";

const MarketplacePage = lazy(async () => {
  const module = await import("./pages/MarketplacePage");
  return { default: module.MarketplacePage };
});

export const marketplaceFeature: FeatureDescriptor = {
  id: "marketplace",
  routes: [{ path: "marketplace", element: <MarketplacePage /> }],
  navigation: {
    labelKey: "nav.marketplace",
    path: "/marketplace",
    icon: Puzzle,
    order: 15,
  },
};
