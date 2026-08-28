import { lazy } from "react";
import { Smartphone } from "lucide-react";
import type { FeatureDescriptor } from "@/app/feature";

const RemotePage = lazy(async () => {
  const module = await import("./pages/RemotePage");
  return { default: module.RemotePage };
});

export const remoteFeature: FeatureDescriptor = {
  id: "remote",
  routes: [{ path: "remote", element: <RemotePage /> }],
  navigation: {
    labelKey: "nav.remote",
    path: "/remote",
    icon: Smartphone,
    order: 12,
  },
};
