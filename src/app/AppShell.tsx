import { Suspense, useEffect } from "react";
import { NavLink, Outlet } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { features } from "@/features/registry";
import { shallowEqual, useLauncherSelector } from "@/platform/launcherStore";
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
      desktopUpdateAvailable:
        snapshot.desktopUpdate.kind === "available" ||
        snapshot.desktopUpdate.kind === "preparing" ||
        snapshot.desktopUpdate.kind === "downloading" ||
        snapshot.desktopUpdate.kind === "installing" ||
        (snapshot.desktopUpdate.kind === "failed" &&
          snapshot.desktopUpdate.version !== null),
    }),
    shallowEqual,
  );
  const { t, i18n } = useTranslation(undefined, { lng: state.language });

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
            <span>
              <strong>{t("app.shortName")}</strong>
              <small>{t("app.subtitle")}</small>
            </span>
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
                {featureId === "settings" && state.desktopUpdateAvailable && (
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
