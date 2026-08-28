import type { FeatureDescriptor } from "@/app/feature";
import { launcherFeature } from "./launcher/descriptor";
import { marketplaceFeature } from "./marketplace/descriptor";
import { remoteFeature } from "./remote/descriptor";
import { settingsFeature } from "./settings/descriptor";

export const features: readonly FeatureDescriptor[] = [
  launcherFeature,
  remoteFeature,
  marketplaceFeature,
  settingsFeature,
];
