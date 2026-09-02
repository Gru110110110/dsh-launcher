import { Suspense, useEffect } from "react";
import { NavLink, Outlet } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { features } from "@/features/registry";
import {
  getDesktopUpdateAction,
  getDesktopUpdateCompactLabel,
  shouldShowDesktopUpdateAction,
} from "@/platform/desktopUpdatePresentation";
import { launcherApi } from "@/platform/launcherApi";
import { shallowEqual, useLauncherSelector } from "@/platform/launcherStore";
import { showTimedError } from "@/shared/errorToast";
import logoUrl from "../../assets/logo-blue.png";
import { ThemeProvider } from "./ThemeProvider";

const navigation = features
  .flatMap((feature) =>
    feature.navigation
      ? [{ ...feature.navigation, featureId: feature.id }]
      : [],
  )
  .sort((left, right) => left.order - right.order);

function ShellContent() {
  const state = useLauncherSelector(
    (snapshot) => ({
      language: snapshot.language,
      theme: snapshot.theme,
      running: snapshot.phase === "ready",
      desktopUpdate: snapshot.desktopUpdate,
    }),
    shallowEqual,
  );
  const { t, i18n } = useTranslation(undefined, { lng: state.language });
  const desktopUpdateAction = getDesktopUpdateAction(state.desktopUpdate);
  const showDesktopUpdate = shouldShowDesktopUpdateAction(state.desktopUpdate);
  const desktopUpdateAccessibleLabel = t(
    desktopUpdateAction.label.key,
    desktopUpdateAction.label.values,
  );
  const desktopUpdateCompactLabel = getDesktopUpdateCompactLabel(
    state.desktopUpdate,
  );

  useEffect(() => {
    if (i18n.language !== state.language)
      void i18n.changeLanguage(state.language);
    document.documentElement.lang = state.language === "zh" ? "zh-CN" : "en";
  }, [i18n, state.language]);

  return (
    <ThemeProvider theme={state.theme}>
      <div className="app-shell">
        <aside className="sidebar">
          <div className="brand" aria-label={t("app.name")}>
            <img src={logoUrl} alt="" className="brand-logo" />
            <span className="brand-copy">
              <strong>{t("app.shortName")}</strong>
              <small>{t("app.subtitle")}</small>
            </span>
            {showDesktopUpdate && (
              <button
                className="sidebar-update-button"
                type="button"
                title={desktopUpdateAccessibleLabel}
                aria-label={desktopUpdateAccessibleLabel}
                disabled={desktopUpdateAction.disabled}
                onClick={() => {
                  if (desktopUpdateAction.operation !== "install") return;
                  void launcherApi
                    .installDesktopUpdate()
                    .catch((error: unknown) => {
                      showTimedError(error, (key, values) => t(key, values));
                    });
                }}
              >
                {desktopUpdateCompactLabel
                  ? t(
                      desktopUpdateCompactLabel.key,
                      desktopUpdateCompactLabel.values,
                    )
                  : null}
              </button>
            )}
          </div>

          <span className="sidebar-caption">{t("nav.menu")}</span>
          <nav aria-label={t("nav.menu")}>
            {navigation.map(({ path, labelKey, icon: Icon, featureId }) => (
              <NavLink
                key={path}
                to={path}
                className={({ isActive }) =>
                  `nav-link${isActive ? " active" : ""}`
                }
              >
                <Icon size={17} strokeWidth={1.8} aria-hidden />
                <span>{t(labelKey)}</span>
                {featureId === "settings" && showDesktopUpdate && (
                  <span
                    className="nav-update-dot"
                    title={t("nav.desktopUpdateAvailable")}
                    aria-label={t("nav.desktopUpdateAvailable")}
                  />
                )}
              </NavLink>
            ))}
          </nav>

          <div className="sidebar-status" aria-live="polite">
            <span className={`status-dot${state.running ? " running" : ""}`} />
            <span>
              {state.running ? t("sidebar.running") : t("sidebar.notRunning")}
            </span>
          </div>
        </aside>
        <main className="main-content">
          <Suspense
            fallback={<div className="route-loading" aria-label="Loading" />}
          >
            <Outlet />
          </Suspense>
        </main>
      </div>
    </ThemeProvider>
  );
}

export function AppShell() {
  return <ShellContent />;
}
