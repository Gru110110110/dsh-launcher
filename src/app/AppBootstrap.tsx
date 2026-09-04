import {
  Component,
  Suspense,
  type ErrorInfo,
  type PropsWithChildren,
} from "react";
import { RouterProvider } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { Toaster } from "sonner";
import { router } from "./router";

class AppErrorBoundary extends Component<
  PropsWithChildren<{ failureMessage: string }>,
  { failed: boolean }
> {
  override state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  override componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("Application render failed", error, info.componentStack);
  }
  override render() {
    if (this.state.failed) {
      return (
        <div className="fatal-error">
          <h1>DSH Launcher</h1>
          <p>{this.props.failureMessage}</p>
        </div>
      );
    }
    return this.props.children;
  }
}

export function AppBootstrap() {
  const { t } = useTranslation();
  return (
    <AppErrorBoundary failureMessage={t("app.loadFailed")}>
      <Suspense
        fallback={<div className="app-loading" aria-label={t("app.loading")} />}
      >
        <RouterProvider router={router} />
        <Toaster
          richColors
          closeButton
          position="top-right"
          containerAriaLabel={t("app.notifications")}
          toastOptions={{ closeButtonAriaLabel: t("action.closeNotification") }}
        />
      </Suspense>
    </AppErrorBoundary>
  );
}
