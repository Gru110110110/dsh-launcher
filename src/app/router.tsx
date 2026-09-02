import { lazy } from "react";
import { Navigate, createHashRouter } from "react-router-dom";
import { AppShell } from "./AppShell";
import { features } from "@/features/registry";

const PetWindow = lazy(async () => {
  const module = await import("@/features/pet/pages/PetWindow");
  return { default: module.PetWindow };
});

export const router = createHashRouter([
  { path: "/pet-window", element: <PetWindow /> },
  {
    path: "/",
    element: <AppShell />,
    children: [
      { index: true, element: <Navigate to="/launcher" replace /> },
      ...features.flatMap((feature) => feature.routes),
      { path: "*", element: <Navigate to="/launcher" replace /> },
    ],
  },
]);
