import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  ChevronDown,
  Copy,
  ExternalLink,
  Info,
  Link2,
  LoaderCircle,
  Power,
  RefreshCw,
  RotateCw,
  ShieldCheck,
  Square,
} from "lucide-react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import deepseekIconUrl from "../../../../assets/external/deepseek.png";
import githubIconUrl from "../../../../assets/external/github.svg";
import { launcherApi } from "@/platform/launcherApi";
import { useLauncherSnapshot } from "@/platform/launcherStore";
import { showMigrationWarning, showTimedError } from "@/shared/errorToast";
import { presentError } from "@/shared/presentError";
import { formatDuration } from "@/shared/time";
import { useNow } from "@/shared/useNow";
import { getDashboardSections } from "../balancePresentation";
import { getHarnessUpdateNotice, getServiceCopy } from "../presentation";
import { BalanceCard } from "./BalanceCard";

export function LauncherPage() {
  const snapshot = useLauncherSnapshot();
  const { t } = useTranslation(undefined, { lng: snapshot.language });
  const now = useNow();
  const running = snapshot.phase === "ready";
  const stopped = snapshot.phase === "stopped";
  const failed = snapshot.phase === "failed";
  const busy = !running && !stopped && !failed;
  const serviceCopy = getServiceCopy(snapshot);
  const updateNotice = getHarnessUpdateNotice(snapshot);
  const backgroundUpdating = snapshot.harnessUpdate.kind === "downloading";
  const [updateChoiceVersion, setUpdateChoiceVersion] = useState<string | null>(
    null,
  );
  const [downloadedPromptVersion, setDownloadedPromptVersion] = useState<
    string | null
  >(null);
  const promptedDownload = useRef<string | null>(null);
  const updateRequestPendingRef = useRef(false);
  const [updateRequestPending, setUpdateRequestPending] = useState(false);
  const marketplaceBusy = snapshot.marketBusy;
  const harnessUpdateBlocked =
    (!running && !stopped) ||
    snapshot.desktopUpdate.kind === "checking" ||
    snapshot.desktopUpdate.kind === "preparing" ||
    snapshot.desktopUpdate.kind === "downloading" ||
    snapshot.desktopUpdate.kind === "installing";
  const selectedBrowser = snapshot.browsers.find(
    (browser) => browser.id === snapshot.selectedBrowserId,
  );
  const elapsed = snapshot.serviceStartedAtMs
    ? formatDuration(now - snapshot.serviceStartedAtMs)
    : null;
  const activityElapsed = snapshot.activity
    ? formatDuration(now - snapshot.activity.startedAtMs)
    : null;
  const activityText = snapshot.activity
    ? t(
        snapshot.activity.values.status === "waiting"
          ? "activity.installingHarnessWaiting"
          : `activity.${snapshot.activity.code}`,
        {
          ...snapshot.activity.values,
          elapsed: activityElapsed,
        },
      )
    : null;
  const progressPercent =
    snapshot.progress.kind === "determinate" && snapshot.progress.total > 0
      ? Math.min(
          100,
          Math.floor((snapshot.progress.done * 100) / snapshot.progress.total),
        )
      : null;
  const showProgress =
    busy &&
    snapshot.activity !== null &&
    snapshot.phase !== "awaitingMigration";
  const migrationPlan =
    snapshot.migration.kind === "pending" ? snapshot.migration.plan : null;
  const migrationWarning = useMemo(() => {
    if (snapshot.migration.kind !== "completedWithWarning") return null;
    return presentError(snapshot.migration.warning, (key, values) =>
      t(key, values),
    );
  }, [snapshot.migration, t]);

  useEffect(() => {
    if (migrationWarning) showMigrationWarning(migrationWarning);
  }, [migrationWarning]);

  useEffect(() => {
    if (
      snapshot.harnessUpdate.kind === "downloaded" &&
      promptedDownload.current !== snapshot.harnessUpdate.version
    ) {
      promptedDownload.current = snapshot.harnessUpdate.version;
      setDownloadedPromptVersion(snapshot.harnessUpdate.version);
    } else if (snapshot.harnessUpdate.kind !== "downloaded") {
      promptedDownload.current = null;
      setDownloadedPromptVersion(null);
    }
  }, [snapshot.harnessUpdate]);

  useEffect(() => {
    if (!updateChoiceVersion) return;
    const current = snapshot.harnessUpdate;
    if (
      !(
        (current.kind === "available" || current.kind === "failed") &&
        current.version === updateChoiceVersion
      )
    ) {
      setUpdateChoiceVersion(null);
    }
  }, [snapshot.harnessUpdate, updateChoiceVersion]);

  const run = (task: Promise<unknown>) => {
    void task.catch((error: unknown) => {
      showTimedError(error, (key, values) => t(key, values));
    });
  };
  const runUpdateRequest = (task: () => Promise<unknown>) => {
    if (updateRequestPendingRef.current) return;
    updateRequestPendingRef.current = true;
    setUpdateRequestPending(true);
    void task()
      .catch((error: unknown) => {
        showTimedError(error, (key, values) => t(key, values));
      })
      .finally(() => {
        updateRequestPendingRef.current = false;
        setUpdateRequestPending(false);
      });
  };
  const checkHarnessUpdate = () => {
    run(
      launcherApi.checkHarnessUpdate().then((version) => {
        if (!version) toast.success(t("update.harness.latest"));
      }),
    );
  };
  const browserName =
    selectedBrowser?.id === "system"
      ? t("browser.default")
      : (selectedBrowser?.label ?? t("browser.default"));

  const sections = getDashboardSections(snapshot.showBalanceCard);

  const serviceSection = (
    <section className="page-section">
      <h2 className="section-label">{t("dashboard.serviceSection")}</h2>
      <div className="panel rows-panel">
        <div className="info-row">
          <Activity className="row-icon" size={18} aria-hidden />
          <div className="row-copy">
            <strong>{t("dashboard.runtime")}</strong>
            <span>
              {running && elapsed
                ? t("dashboard.runtimeDetail", { time: elapsed })
                : stopped
                  ? t("dashboard.stoppedDetail")
                  : failed && snapshot.error
                    ? presentError(snapshot.error, (key, values) =>
                        t(key, values),
                      )
                    : (activityText ?? t(serviceCopy.title))}
            </span>
            {showProgress && (
              <div
                className="runtime-progress"
                role="progressbar"
                aria-label={activityText ?? t(serviceCopy.title)}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={progressPercent ?? undefined}
              >
                <div
                  className={`runtime-progress-track${progressPercent === null ? " indeterminate" : ""}`}
                >
                  <i
                    style={
                      progressPercent === null
                        ? undefined
                        : { width: `${String(progressPercent)}%` }
                    }
                  />
                </div>
                {progressPercent !== null && (
                  <small>{String(progressPercent)}%</small>
                )}
              </div>
            )}
          </div>
          <div className="row-actions">
            {running ? (
              <>
                <button
                  className="outline-button danger"
                  disabled={marketplaceBusy}
                  title={
                    marketplaceBusy ? t("market.operationBusy") : undefined
                  }
                  onClick={() => {
                    run(launcherApi.stop());
                  }}
                >
                  <Square size={11} fill="currentColor" />
                  {t("action.stop")}
                </button>
                <button
                  className="outline-button"
                  disabled={marketplaceBusy}
                  title={
                    marketplaceBusy ? t("market.operationBusy") : undefined
                  }
                  onClick={() => {
                    run(launcherApi.restart());
                  }}
                >
                  <RotateCw size={14} />
                  {t("action.restart")}
                </button>
              </>
            ) : (
              <button
                className="primary-button"
                disabled={busy}
                onClick={() => {
                  run(launcherApi.retry());
                }}
              >
                {busy ? (
                  <LoaderCircle className="spin" size={14} />
                ) : (
                  <Power size={14} />
                )}
                {busy ? t(serviceCopy.busyAction) : t("action.start")}
              </button>
            )}
          </div>
        </div>

        <div className="info-row">
          <Link2 className="row-icon" size={18} aria-hidden />
          <div className="row-copy">
            <strong>{t("dashboard.address")}</strong>
            <span>{t("dashboard.addressDetail")}</span>
          </div>
          <div className="address-actions">
            <span className="service-url">
              {snapshot.webUrl ?? t("service.waitingAddress")}
            </span>
            <button
              className="icon-button"
              type="button"
              disabled={!snapshot.webUrl}
              aria-label={t("action.copy")}
              onClick={() => {
                run(
                  launcherApi
                    .copyWebUrl()
                    .then(() => toast.success(t("action.copied"))),
                );
              }}
            >
              <Copy size={15} />
            </button>
            <div className="split-button">
              <button
                className="primary-button open-button"
                disabled={!running}
                onClick={() => {
                  run(launcherApi.openWebUi());
                }}
              >
                <ExternalLink size={15} />
                {t("action.openWith", { browser: browserName })}
              </button>
              {snapshot.browsers.length > 1 && (
                <DropdownMenu.Root>
                  <DropdownMenu.Trigger
                    className="split-menu"
                    disabled={!running}
                    aria-label={t("action.chooseBrowser")}
                  >
                    <ChevronDown size={16} />
                  </DropdownMenu.Trigger>
                  <DropdownMenu.Portal>
                    <DropdownMenu.Content
                      className="dropdown-content"
                      align="end"
                      sideOffset={6}
                    >
                      <DropdownMenu.RadioGroup
                        value={snapshot.selectedBrowserId}
                        onValueChange={(id) => {
                          run(launcherApi.selectBrowser(id));
                        }}
                      >
                        {snapshot.browsers.map((browser) => (
                          <DropdownMenu.RadioItem
                            className="dropdown-item"
                            value={browser.id}
                            key={browser.id}
                          >
                            <DropdownMenu.ItemIndicator className="dropdown-indicator">
                              ✓
                            </DropdownMenu.ItemIndicator>
                            {browser.id === "system"
                              ? t("browser.default")
                              : browser.label}
                          </DropdownMenu.RadioItem>
                        ))}
                      </DropdownMenu.RadioGroup>
                    </DropdownMenu.Content>
                  </DropdownMenu.Portal>
                </DropdownMenu.Root>
              )}
            </div>
          </div>
        </div>
      </div>
    </section>
  );

  const resourcesSection = (
    <section className="page-section resources-section">
      <h2 className="section-label">{t("dashboard.resourcesSection")}</h2>
      <div className="panel rows-panel resource-panel">
        <button
          className="info-row resource-row"
          type="button"
          onClick={() => {
            run(launcherApi.openExternalLink("deepseek"));
          }}
        >
          <img className="resource-icon" src={deepseekIconUrl} alt="" />
          <span className="row-copy">
            <strong>{t("resource.deepseek")}</strong>
            <span>{t("resource.deepseekDetail")}</span>
          </span>
          <span className="resource-target">platform.deepseek.com</span>
          <span className="icon-button" aria-hidden>
            <ExternalLink size={14} />
          </span>
        </button>
        <button
          className="info-row resource-row"
          type="button"
          onClick={() => {
            run(launcherApi.openExternalLink("harnessGithub"));
          }}
        >
          <img
            className="resource-icon github-icon"
            src={githubIconUrl}
            alt=""
          />
          <span className="row-copy">
            <strong>GitHub</strong>
            <span>{t("resource.harnessGithubDetail")}</span>
          </span>
          <span className="resource-target">
            github.com/deepseek-ai/deepseek-harness
          </span>
          <span className="icon-button" aria-hidden>
            <ExternalLink size={14} />
          </span>
        </button>
      </div>
    </section>
  );

  return (
    <section className="content-page">
      <header className="page-header">
        <h1>{t("dashboard.title")}</h1>
        <p>{t("dashboard.subtitle")}</p>
      </header>

      <div className="panel product-panel">
        <div className="app-icon-box">
          <img src={deepseekIconUrl} alt="" />
        </div>
        <div className="product-copy">
          <div className="product-title-line">
            <h2>DeepSeek Harness</h2>
            <span
              className={`status-pill ${running ? "success" : failed ? "danger" : "busy"}`}
            >
              {running ? (
                <span className="status-dot running" />
              ) : busy ? (
                <LoaderCircle className="spin" size={13} />
              ) : null}
              {running
                ? t("service.running")
                : stopped
                  ? t("service.stopped")
                  : t(serviceCopy.badge)}
            </span>
          </div>
          <div className="product-version">
            <span>v{snapshot.harnessVersion ?? "—"}</span>
            <button
              type="button"
              className="inline-action"
              disabled={
                (!running && !stopped) ||
                !snapshot.harnessVersion ||
                snapshot.harnessUpdate.kind === "checking" ||
                snapshot.harnessUpdate.kind === "downloading" ||
                snapshot.harnessUpdate.kind === "installing"
              }
              onClick={checkHarnessUpdate}
            >
              <RefreshCw
                size={13}
                className={
                  snapshot.harnessUpdate.kind === "checking" ||
                  backgroundUpdating
                    ? "spin"
                    : ""
                }
              />
              {backgroundUpdating
                ? t("action.backgroundUpdating")
                : t("action.checkUpdate")}
            </button>
          </div>
        </div>
      </div>

      {migrationPlan && (
        <div className="migration-card">
          <ShieldCheck size={22} aria-hidden />
          <div>
            <h2>{t("migration.title")}</h2>
            <p>{t("migration.detail")}</p>
            <ul>
              {migrationPlan.sourceEntries > 0 && (
                <li>
                  {t("migration.sourceEntries", {
                    count: migrationPlan.sourceEntries,
                  })}
                </li>
              )}
              {migrationPlan.workspaceAvailable && (
                <li>{t("migration.workspace")}</li>
              )}
              {migrationPlan.ccSwitchProviders > 0 && (
                <li>
                  {t("migration.ccSwitch", {
                    count: migrationPlan.ccSwitchProviders,
                  })}
                </li>
              )}
            </ul>
            <p className="migration-safety">{t("migration.safety")}</p>
            <div className="migration-actions">
              <button
                className="secondary-button"
                onClick={() => {
                  run(launcherApi.skipMigration());
                }}
              >
                {t("action.skipMigration")}
              </button>
              <button
                className="primary-button"
                onClick={() => {
                  run(launcherApi.approveMigration());
                }}
              >
                {t("action.approveMigration")}
              </button>
            </div>
          </div>
        </div>
      )}

      {updateNotice && (
        <div
          className={`update-banner${updateNotice.tone === "error" ? " error" : ""}`}
        >
          <span>
            {t(updateNotice.message.key, updateNotice.message.values)}
          </span>
          <button
            disabled={harnessUpdateBlocked || updateRequestPending}
            onClick={() => {
              if (snapshot.harnessUpdate.kind === "downloaded") {
                setDownloadedPromptVersion(snapshot.harnessUpdate.version);
              } else if (
                snapshot.harnessUpdate.kind === "available" ||
                snapshot.harnessUpdate.kind === "failed"
              ) {
                setUpdateChoiceVersion(snapshot.harnessUpdate.version);
              }
            }}
          >
            {t(updateNotice.actionLabel)}
          </button>
        </div>
      )}

      {updateChoiceVersion && (
        <div
          className="modal-overlay"
          role="presentation"
          onMouseDown={(event) => {
            if (event.currentTarget === event.target) {
              setUpdateChoiceVersion(null);
            }
          }}
        >
          <div
            className="update-modal"
            role="dialog"
            aria-modal="true"
            aria-labelledby="harness-update-mode-title"
          >
            <h2 id="harness-update-mode-title">
              {t("update.harness.modeTitle", { version: updateChoiceVersion })}
            </h2>
            <p>{t("update.harness.modeDetail")}</p>
            <div className="update-mode-options">
              <button
                className="update-mode-option"
                type="button"
                disabled={updateRequestPending}
                onClick={() => {
                  const version = updateChoiceVersion;
                  setUpdateChoiceVersion(null);
                  runUpdateRequest(() =>
                    launcherApi.updateHarness("background", version),
                  );
                }}
              >
                <strong>{t("update.harness.backgroundTitle")}</strong>
                <span>{t("update.harness.backgroundDetail")}</span>
              </button>
              <button
                className="update-mode-option"
                type="button"
                disabled={updateRequestPending}
                onClick={() => {
                  const version = updateChoiceVersion;
                  setUpdateChoiceVersion(null);
                  runUpdateRequest(() =>
                    launcherApi.updateHarness("foreground", version),
                  );
                }}
              >
                <strong>{t("update.harness.foregroundTitle")}</strong>
                <span>{t("update.harness.foregroundDetail")}</span>
              </button>
            </div>
            <div className="modal-actions">
              <button
                className="secondary-button"
                type="button"
                disabled={updateRequestPending}
                onClick={() => {
                  setUpdateChoiceVersion(null);
                }}
              >
                {t("action.cancel")}
              </button>
            </div>
          </div>
        </div>
      )}

      {downloadedPromptVersion && (
        <div className="modal-overlay" role="presentation">
          <div
            className="update-modal"
            role="alertdialog"
            aria-modal="true"
            aria-labelledby="harness-update-ready-title"
            aria-describedby="harness-update-ready-detail"
          >
            <h2 id="harness-update-ready-title">
              {t("update.harness.downloadCompleteTitle")}
            </h2>
            <p id="harness-update-ready-detail">
              {t("update.harness.downloadCompleteDetail", {
                version: downloadedPromptVersion,
              })}
            </p>
            <div className="modal-actions">
              <button
                className="secondary-button"
                type="button"
                disabled={updateRequestPending}
                onClick={() => {
                  setDownloadedPromptVersion(null);
                }}
              >
                {t("action.later")}
              </button>
              <button
                className="primary-button"
                type="button"
                disabled={updateRequestPending}
                onClick={() => {
                  const version = downloadedPromptVersion;
                  setDownloadedPromptVersion(null);
                  runUpdateRequest(() =>
                    launcherApi.activateHarnessUpdate(version),
                  );
                }}
              >
                {t("action.confirmRestartAndUpdate")}
              </button>
            </div>
          </div>
        </div>
      )}

      {sections.map((section) => {
        if (section === "service") {
          return <Fragment key="service">{serviceSection}</Fragment>;
        }
        if (section === "balance") {
          return <BalanceCard key="balance" />;
        }
        return <Fragment key="resources">{resourcesSection}</Fragment>;
      })}

      <p className="page-footnote">
        <Info size={15} strokeWidth={1.8} aria-hidden />
        {t(snapshot.trayAvailable ? "footer.closeHint" : "footer.noTray")}
      </p>
    </section>
  );
}
