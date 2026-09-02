import {
  ExternalLink,
  Globe2,
  HardDrive,
  Info,
  Languages,
  MoonStar,
  Network,
  RefreshCw,
  Trash2,
  Wallet,
} from "lucide-react";
import {
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { launcherApi } from "@/platform/launcherApi";
import {
  getDesktopUpdateAction,
  getDesktopUpdateDetail,
} from "@/platform/desktopUpdatePresentation";
import { shallowEqual, useLauncherSelector } from "@/platform/launcherStore";
import type {
  HarnessUpdateChannel,
  Language,
  ProxyMode,
  ProxySettings,
  ProxyTestReport,
  StartupRepairBackupSummary,
  ThemePreference,
} from "@/platform/generated/bindings";
import { showTimedError } from "@/shared/errorToast";
import githubIconUrl from "../../../../assets/external/github.svg";
import {
  proxyDraftAfterSave,
  proxyDraftChanged,
  proxyDraftFromSettings,
  proxySettingsFromDraft,
  proxyTestFailureCopy,
  proxyTestSuccessCopy,
  proxyValidationErrorKey,
  validateProxyDraft,
  type ProxyDraft,
} from "../presentation";

function SegmentedControl<T extends string>({
  value,
  options,
  onChange,
  label,
  disabled = false,
}: {
  value: T;
  options: readonly { value: T; label: string }[];
  onChange: (value: T) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <div className="segmented-control" role="radiogroup" aria-label={label}>
      {options.map((option) => (
        <button
          type="button"
          role="radio"
          aria-checked={option.value === value}
          className={option.value === value ? "selected" : ""}
          disabled={disabled}
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

function proxyErrorMessage(
  error: unknown,
  t: (key: string) => string,
): string | null {
  if (
    typeof error === "object" &&
    error !== null &&
    (error as { code?: unknown }).code === "proxyUrlInvalid"
  ) {
    const values = (error as { values?: Record<string, unknown> }).values;
    const reason = typeof values?.reason === "string" ? values.reason : null;
    return t(proxyValidationErrorKey(reason));
  }
  return null;
}

function revealFullTextOnTruncation(
  event: ReactMouseEvent<HTMLSpanElement>,
  fullText: string,
) {
  const element = event.currentTarget;
  // Only surface the native tooltip when the ellipsis actually hides content.
  element.title = element.scrollWidth > element.clientWidth ? fullText : "";
}

function formatBackupBytes(bytes: number): string {
  if (bytes < 1024) return `${String(bytes)} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function ProxySection({
  proxy,
  run,
  t,
}: {
  proxy: ProxySettings;
  run: (task: Promise<unknown>) => void;
  t: (key: string, values?: Record<string, unknown>) => string;
}) {
  const [draft, setDraft] = useState<ProxyDraft>(() =>
    proxyDraftFromSettings(proxy),
  );
  const [formError, setFormError] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  const [report, setReport] = useState<ProxyTestReport | null>(null);
  const testRequest = useRef(0);
  const draftRevision = useRef(0);
  const draftError = validateProxyDraft(draft);
  const dirty = proxyDraftChanged(draft, proxy);
  const modeDetail: Record<ProxyMode, string> = {
    system: "settings.proxyModeSystemDetail",
    direct: "settings.proxyModeDirectDetail",
    manual: "settings.proxyModeManualDetail",
  };
  const successCopy = report ? proxyTestSuccessCopy(report) : null;
  const proxyNote = `${t("settings.proxyCredentialsNote")} ${t("settings.proxyTestNote")}`;

  const update = (patch: Partial<ProxyDraft>) => {
    // Invalidate any in-flight connection test. Its result belongs to the old
    // form values and must never be rendered under the updated draft.
    testRequest.current += 1;
    draftRevision.current += 1;
    setDraft((current) => ({ ...current, ...patch }));
    setFormError(null);
    setReport(null);
    setTesting(false);
  };

  const save = () => {
    if (draftError) {
      setFormError(t(proxyValidationErrorKey(draftError)));
      return;
    }
    setFormError(null);
    const settings = proxySettingsFromDraft(draft);
    const saveRevision = draftRevision.current;
    run(
      launcherApi
        .setProxy(settings)
        .then((restartRequired) => {
          // Match the backend's canonical persistence representation. Inactive
          // manual fields are not retained after Direct/System is saved.
          setDraft((current) =>
            proxyDraftAfterSave(
              current,
              settings,
              saveRevision,
              draftRevision.current,
            ),
          );
          if (!restartRequired) {
            toast.success(t("settings.proxySaved"));
            return;
          }
          toast.success(t("settings.proxySavedRestartRequired"), {
            duration: 12_000,
            action: {
              label: t("settings.proxyRestart"),
              onClick: () => {
                run(launcherApi.restart());
              },
            },
          });
        })
        .catch((error: unknown) => {
          const message = proxyErrorMessage(error, t);
          if (message) {
            if (draftRevision.current === saveRevision) {
              setFormError(message);
            }
            return;
          }
          throw error;
        }),
    );
  };

  const test = () => {
    if (draftError) {
      setFormError(t(proxyValidationErrorKey(draftError)));
      return;
    }
    setFormError(null);
    setReport(null);
    setTesting(true);
    const request = ++testRequest.current;
    launcherApi
      .testProxy(proxySettingsFromDraft(draft))
      .then((result) => {
        if (testRequest.current !== request) return;
        setReport(result);
      })
      .catch((error: unknown) => {
        if (testRequest.current !== request) return;
        const message = proxyErrorMessage(error, t);
        if (message) {
          setFormError(message);
        } else {
          showTimedError(error, (key, values) => t(key, values));
        }
      })
      .finally(() => {
        if (testRequest.current !== request) return;
        setTesting(false);
      });
  };

  return (
    <section className="page-section settings-proxy">
      <h2 className="section-label">{t("settings.proxy")}</h2>
      <div className="panel rows-panel">
        <div className="info-row settings-row">
          <Network className="row-icon" size={18} aria-hidden />
          <div className="row-copy">
            <strong>{t("settings.proxy")}</strong>
            <span>{t(modeDetail[draft.mode])}</span>
          </div>
          <SegmentedControl<ProxyMode>
            label={t("settings.proxy")}
            value={draft.mode}
            options={[
              { value: "system", label: t("settings.proxyModeSystem") },
              { value: "direct", label: t("settings.proxyModeDirect") },
              { value: "manual", label: t("settings.proxyModeManual") },
            ]}
            onChange={(mode) => {
              update({ mode });
            }}
          />
        </div>
        {draft.mode === "manual" && (
          <>
            <div className="info-row settings-row proxy-field-row">
              <div className="proxy-field">
                <label htmlFor="proxy-url">{t("settings.proxyUrl")}</label>
                <input
                  id="proxy-url"
                  type="text"
                  value={draft.url}
                  placeholder={t("settings.proxyUrlPlaceholder")}
                  spellCheck={false}
                  autoComplete="off"
                  onChange={(event) => {
                    update({ url: event.target.value });
                  }}
                />
              </div>
            </div>
            <div className="info-row settings-row proxy-field-row">
              <div className="proxy-field">
                <label htmlFor="proxy-bypass">
                  {t("settings.proxyBypass")}
                </label>
                <input
                  id="proxy-bypass"
                  type="text"
                  value={draft.bypass}
                  placeholder={t("settings.proxyBypassPlaceholder")}
                  spellCheck={false}
                  autoComplete="off"
                  onChange={(event) => {
                    update({ bypass: event.target.value });
                  }}
                />
                <span className="proxy-hint">
                  {t("settings.proxyBypassDetail")}
                </span>
              </div>
            </div>
          </>
        )}
        <div className="info-row settings-row proxy-actions-row">
          <div className="row-copy">
            <span
              className="proxy-hint"
              onMouseEnter={(event) => {
                revealFullTextOnTruncation(event, proxyNote);
              }}
            >
              {proxyNote}
            </span>
            {formError && <span className="proxy-error">{formError}</span>}
          </div>
          <button
            className="outline-button"
            type="button"
            disabled={testing || draftError !== null}
            onClick={test}
          >
            <RefreshCw size={14} className={testing ? "spin" : ""} />
            {testing ? t("settings.proxyTesting") : t("settings.proxyTest")}
          </button>
          <button
            className="primary-button"
            type="button"
            disabled={!dirty || draftError !== null}
            onClick={save}
          >
            {t("settings.proxySave")}
          </button>
        </div>
        {report && (
          <div className="proxy-report" role="status">
            {successCopy && (
              <span className="proxy-report-success">
                {t(successCopy.key, successCopy.values)}
              </span>
            )}
            {report.sources.length === 0 && (
              <span className="proxy-error">
                {t("settings.proxyTestFailed")}
              </span>
            )}
            {report.failures.map((failure) => {
              const copy = proxyTestFailureCopy(failure);
              return (
                <span className="proxy-report-failure" key={failure.source}>
                  {t(copy.key, copy.values)} — {failure.detail}
                </span>
              );
            })}
          </div>
        )}
      </div>
    </section>
  );
}

export function SettingsPage() {
  const state = useLauncherSelector(
    (snapshot) => ({
      language: snapshot.language,
      theme: snapshot.theme,
      desktopVersion: snapshot.desktopVersion,
      showBalanceCard: snapshot.showBalanceCard,
      harnessUpdateChannel: snapshot.harnessUpdateChannel,
      harnessUpdateChannelLocked: [
        "downloading",
        "downloaded",
        "installing",
      ].includes(snapshot.harnessUpdate.kind),
    }),
    shallowEqual,
  );
  const proxy = useLauncherSelector((snapshot) => snapshot.proxy, shallowEqual);
  const desktopUpdate = useLauncherSelector(
    (snapshot) => snapshot.desktopUpdate,
    shallowEqual,
  );
  const { t } = useTranslation(undefined, { lng: state.language });
  const desktopUpdateDetail = getDesktopUpdateDetail(desktopUpdate);
  const desktopUpdateAction = getDesktopUpdateAction(desktopUpdate);
  const [repairBackups, setRepairBackups] =
    useState<StartupRepairBackupSummary | null>(null);
  const [repairBackupsLoading, setRepairBackupsLoading] = useState(true);
  const [repairBackupsClearing, setRepairBackupsClearing] = useState(false);
  const [repairBackupsConfirming, setRepairBackupsConfirming] = useState(false);
  const repairBackupsCancelButton = useRef<HTMLButtonElement>(null);
  const run = (task: Promise<unknown>) => {
    void task.catch((error: unknown) => {
      showTimedError(error, (key, values) => t(key, values));
    });
  };

  useEffect(() => {
    let current = true;
    setRepairBackupsLoading(true);
    void launcherApi
      .startupRepairBackups()
      .then((summary) => {
        if (current) setRepairBackups(summary);
      })
      .catch((error: unknown) => {
        if (current) {
          showTimedError(error, (key, values) => t(key, values));
        }
      })
      .finally(() => {
        if (current) setRepairBackupsLoading(false);
      });
    return () => {
      current = false;
    };
  }, [t]);

  useEffect(() => {
    if (!repairBackupsConfirming) return;
    repairBackupsCancelButton.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setRepairBackupsConfirming(false);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [repairBackupsConfirming]);

  const clearRepairBackups = () => {
    setRepairBackupsConfirming(false);
    setRepairBackupsClearing(true);
    void launcherApi
      .clearStartupRepairBackups()
      .then((summary) => {
        setRepairBackups(summary);
        toast.success(t("settings.repairBackupsCleared"));
      })
      .catch((error: unknown) => {
        showTimedError(error, (key, values) => t(key, values));
      })
      .finally(() => {
        setRepairBackupsClearing(false);
      });
  };

  const repairBackupDetail = repairBackupsLoading
    ? t("settings.repairBackupsLoading")
    : repairBackups && repairBackups.count > 0
      ? t("settings.repairBackupsDetail", {
          count: repairBackups.count,
          size: formatBackupBytes(repairBackups.totalBytes),
          date: repairBackups.nextExpiryAtMs
            ? new Date(repairBackups.nextExpiryAtMs).toLocaleDateString(
                state.language === "zh" ? "zh-CN" : "en-US",
              )
            : t("settings.repairBackupsUnknownExpiry"),
        })
      : t("settings.repairBackupsEmpty");

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
            <RefreshCw className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("settings.harnessUpdateChannel")}</strong>
              <span>
                {t(
                  state.harnessUpdateChannel === "alpha"
                    ? "settings.harnessUpdateChannelAlphaDetail"
                    : "settings.harnessUpdateChannelLatestDetail",
                )}
              </span>
            </div>
            <SegmentedControl<HarnessUpdateChannel>
              label={t("settings.harnessUpdateChannel")}
              value={state.harnessUpdateChannel}
              disabled={state.harnessUpdateChannelLocked}
              options={[
                {
                  value: "latest",
                  label: t("settings.harnessUpdateChannelLatest"),
                },
                {
                  value: "alpha",
                  label: t("settings.harnessUpdateChannelAlpha"),
                },
              ]}
              onChange={(channel) => {
                run(launcherApi.setHarnessUpdateChannel(channel));
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

      <ProxySection proxy={proxy} run={run} t={t} />

      <section className="page-section settings-storage">
        <h2 className="section-label">{t("settings.storage")}</h2>
        <div className="panel rows-panel">
          <div className="info-row settings-row">
            <HardDrive className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("settings.repairBackups")}</strong>
              <span>{repairBackupDetail}</span>
              {repairBackups && repairBackups.protectedCount > 0 && (
                <span className="settings-backup-warning">
                  {t("settings.repairBackupsProtected", {
                    count: repairBackups.protectedCount,
                  })}
                </span>
              )}
            </div>
            <button
              className="outline-button danger"
              type="button"
              disabled={
                repairBackupsLoading ||
                repairBackupsClearing ||
                !repairBackups ||
                repairBackups.count === 0
              }
              onClick={() => {
                setRepairBackupsConfirming(true);
              }}
            >
              <Trash2 size={14} />
              {repairBackupsClearing
                ? t("settings.repairBackupsClearing")
                : t("settings.repairBackupsClear")}
            </button>
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

      {repairBackupsConfirming && (
        <div
          className="modal-overlay"
          role="presentation"
          onClick={() => {
            setRepairBackupsConfirming(false);
          }}
        >
          <div
            className="update-modal"
            role="alertdialog"
            aria-modal="true"
            aria-labelledby="repair-backups-clear-title"
            aria-describedby="repair-backups-clear-detail"
            onClick={(event) => {
              event.stopPropagation();
            }}
          >
            <h2 id="repair-backups-clear-title">
              {t("settings.repairBackupsClearTitle")}
            </h2>
            <p id="repair-backups-clear-detail">
              {t("settings.repairBackupsClearConfirm")}
            </p>
            <div className="modal-actions">
              <button
                ref={repairBackupsCancelButton}
                className="outline-button"
                type="button"
                onClick={() => {
                  setRepairBackupsConfirming(false);
                }}
              >
                {t("action.cancel")}
              </button>
              <button
                className="primary-button danger-button"
                type="button"
                onClick={clearRepairBackups}
              >
                {t("settings.repairBackupsClear")}
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
