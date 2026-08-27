import {
  ExternalLink,
  Globe2,
  Info,
  Languages,
  MoonStar,
  RefreshCw,
  Wallet,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { launcherApi } from "@/platform/launcherApi";
import { shallowEqual, useLauncherSelector } from "@/platform/launcherStore";
import type { Language, ThemePreference } from "@/platform/generated/bindings";
import { showTimedError } from "@/shared/errorToast";
import githubIconUrl from "../../../../assets/external/github.svg";
import {
  getDesktopUpdateAction,
  getDesktopUpdateDetail,
} from "../presentation";

function SegmentedControl<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: readonly { value: T; label: string }[];
  onChange: (value: T) => void;
  label: string;
}) {
  return (
    <div className="segmented-control" role="radiogroup" aria-label={label}>
      {options.map((option) => (
        <button
          type="button"
          role="radio"
          aria-checked={option.value === value}
          className={option.value === value ? "selected" : ""}
          key={option.value}
          onClick={() => {
            onChange(option.value);
          }}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

export function SettingsPage() {
  const state = useLauncherSelector(
    (snapshot) => ({
      language: snapshot.language,
      theme: snapshot.theme,
      desktopVersion: snapshot.desktopVersion,
      showBalanceCard: snapshot.showBalanceCard,
    }),
    shallowEqual,
  );
  const desktopUpdate = useLauncherSelector(
    (snapshot) => snapshot.desktopUpdate,
    shallowEqual,
  );
  const { t } = useTranslation(undefined, { lng: state.language });
  const desktopUpdateDetail = getDesktopUpdateDetail(desktopUpdate);
  const desktopUpdateAction = getDesktopUpdateAction(desktopUpdate);
  const run = (task: Promise<unknown>) => {
    void task.catch((error: unknown) => {
      showTimedError(error, (key, values) => t(key, values));
    });
  };

  return (
    <section className="content-page">
      <header className="page-header">
        <h1>{t("settings.title")}</h1>
        <p>{t("settings.subtitle")}</p>
      </header>

      <section className="page-section settings-general">
        <h2 className="section-label">{t("settings.general")}</h2>
        <div className="panel rows-panel">
          <div className="info-row settings-row">
            <Languages className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("settings.language")}</strong>
              <span>{t("settings.languageDetail")}</span>
            </div>
            <SegmentedControl<Language>
              label={t("settings.language")}
              value={state.language}
              options={[
                { value: "zh", label: "中文" },
                { value: "en", label: "English" },
              ]}
              onChange={(language) => {
                run(launcherApi.setLanguage(language));
              }}
            />
          </div>
          <div className="info-row settings-row">
            <MoonStar className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("settings.theme")}</strong>
              <span>{t("settings.themeDetail")}</span>
            </div>
            <SegmentedControl<ThemePreference>
              label={t("settings.theme")}
              value={state.theme}
              options={[
                { value: "light", label: t("theme.light") },
                { value: "dark", label: t("theme.dark") },
                { value: "system", label: t("theme.system") },
              ]}
              onChange={(theme) => {
                run(launcherApi.setTheme(theme));
              }}
            />
          </div>
          <div className="info-row settings-row">
            <Wallet className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("settings.balanceCard")}</strong>
              <span>{t("settings.balanceCardDetail")}</span>
            </div>
            <SegmentedControl<"show" | "hide">
              label={t("settings.balanceCard")}
              value={state.showBalanceCard ? "show" : "hide"}
              options={[
                { value: "show", label: t("settings.balanceCardShow") },
                { value: "hide", label: t("settings.balanceCardHide") },
              ]}
              onChange={(choice) => {
                run(launcherApi.setShowBalanceCard(choice === "show"));
              }}
            />
          </div>
        </div>
      </section>

      <section className="page-section settings-about">
        <h2 className="section-label">{t("settings.about")}</h2>
        <div className="panel rows-panel">
          <div className="info-row settings-row">
            <Info className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("settings.desktopVersion")}</strong>
              <span>
                {t(desktopUpdateDetail.key, desktopUpdateDetail.values)}
              </span>
            </div>
            <span className="version-text">v{state.desktopVersion}</span>
            <button
              className={`${
                desktopUpdateAction.appearance === "primary"
                  ? "primary-button"
                  : "outline-button"
              } desktop-update-button`}
              type="button"
              disabled={desktopUpdateAction.disabled}
              onClick={() => {
                if (desktopUpdateAction.operation === "install") {
                  run(launcherApi.installDesktopUpdate());
                  return;
                }
                if (desktopUpdateAction.operation !== "check") return;
                run(
                  launcherApi.checkDesktopUpdate().then((version) => {
                    if (!version) toast.success(t("update.desktop.latest"));
                  }),
                );
              }}
            >
              <RefreshCw
                size={14}
                className={desktopUpdateAction.spinning ? "spin" : ""}
              />
              {t(
                desktopUpdateAction.label.key,
                desktopUpdateAction.label.values,
              )}
            </button>
          </div>
          <button
            className="info-row resource-row settings-row"
            type="button"
            onClick={() => {
              run(launcherApi.openWebsite());
            }}
          >
            <Globe2 className="row-icon" size={18} aria-hidden />
            <span className="row-copy">
              <strong>{t("settings.website")}</strong>
              <span>{t("settings.websiteDetail")}</span>
            </span>
            <span className="resource-target">dsdesktop.com</span>
            <span className="icon-button" aria-hidden>
              <ExternalLink size={14} />
            </span>
          </button>
          <button
            className="info-row resource-row settings-row"
            type="button"
            onClick={() => {
              run(launcherApi.openExternalLink("github"));
            }}
          >
            <img
              className="resource-icon github-icon"
              src={githubIconUrl}
              alt=""
            />
            <span className="row-copy">
              <strong>GitHub</strong>
              <span>{t("resource.githubDetail")}</span>
            </span>
            <span className="resource-target">
              github.com/Gru110110110/deepseek-harness-desktop-launcher
            </span>
            <span className="icon-button" aria-hidden>
              <ExternalLink size={14} />
            </span>
          </button>
        </div>
      </section>
    </section>
  );
}
