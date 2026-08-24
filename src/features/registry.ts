import type { FeatureDescriptor } from "@/app/feature";
import { launcherFeature } from "./launcher/descriptor";
import { marketplaceFeature } from "./marketplace/descriptor";
import { settingsFeature } from "./settings/descriptor";

export const features: readonly FeatureDescriptor[] = [
  launcherFeature,
  marketplaceFeature,
  settingsFeature,
];
