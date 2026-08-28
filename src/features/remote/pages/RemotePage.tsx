import {
  Copy,
  Globe2,
  Loader2,
  RefreshCw,
  Smartphone,
  SquarePen,
  Wifi,
} from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { launcherApi } from "@/platform/launcherApi";
import { shallowEqual, useLauncherSelector } from "@/platform/launcherStore";
import type { RemoteScope } from "@/platform/generated/bindings";
import { showTimedError } from "@/shared/errorToast";
import { presentError } from "@/shared/presentError";
import {
  formatRemotePassword,
  isErrorCode,
  isValidRemotePassword,
  publicStateCopy,
  qrDataUrl,
} from "../presentation";

type Translate = (key: string, values?: Record<string, unknown>) => string;

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

function useRemoteQr(scope: RemoteScope, url: string | null): string | null {
  const [svg, setSvg] = useState<string | null>(null);
  useEffect(() => {
    if (!url) {
      setSvg(null);
      return;
    }
    let cancelled = false;
    launcherApi
      .remoteQr(scope)
      .then((result) => {
        if (!cancelled) setSvg(result);
      })
      .catch(() => {
        // The QR is a convenience; the address text below stays usable.
      });
    return () => {
      cancelled = true;
    };
  }, [scope, url]);
  return url ? svg : null;
}

function PasswordEditModal({
  scope,
  t,
  run,
  onClose,
}: {
  scope: RemoteScope;
  t: Translate;
  run: (task: Promise<unknown>) => void;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const confirm = () => {
    if (!isValidRemotePassword(draft)) {
      setError(t("remote.passwordEditInvalid"));
      return;
    }
    run(
      launcherApi.remoteSetPassword(scope, draft).then(() => {
        toast.success(t("remote.passwordSaved"));
      }),
    );
    onClose();
  };
  return (
    <div className="modal-overlay" role="presentation" onClick={onClose}>
      <div
        className="update-modal remote-password-modal"
        role="dialog"
        aria-modal="true"
        aria-label={t("remote.passwordEditTitle")}
        onClick={(event) => {
          event.stopPropagation();
        }}
      >
        <h2>{t("remote.passwordEditTitle")}</h2>
        <p>{t("remote.passwordEditDetail")}</p>
        <input
          className="remote-password-input"
          type="text"
          inputMode="numeric"
          autoComplete="off"
          spellCheck={false}
          maxLength={8}
          placeholder={t("remote.passwordEditPlaceholder")}
          aria-label={t("remote.passwordEditTitle")}
          value={draft}
          autoFocus
          onChange={(event) => {
            setDraft(event.target.value);
            setError(null);
          }}
          onKeyDown={(event) => {
            if (event.key === "Enter") confirm();
          }}
        />
        {error && <span className="proxy-error">{error}</span>}
        <div className="modal-actions">
          <button className="outline-button" type="button" onClick={onClose}>
            {t("action.cancel")}
          </button>
          <button
            className="primary-button"
            type="button"
            disabled={!isValidRemotePassword(draft)}
            onClick={confirm}
          >
            {t("remote.passwordEditConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}

function RemoteLinkPanel({
  scope,
  url,
  password,
  t,
  run,
}: {
  scope: RemoteScope;
  url: string;
  password: string;
  t: Translate;
  run: (task: Promise<unknown>) => void;
}) {
  const svg = useRemoteQr(scope, url);
  const [editing, setEditing] = useState(false);
  const copyAddress = () => {
    void navigator.clipboard
      .writeText(url)
      .then(() => {
        toast.success(t("action.copied"));
      })
      .catch(() => {
        toast.error(t("error.unknown"));
      });
  };
  return (
    <div className="remote-link-panel">
      <div className="remote-qr">
        {svg ? (
          <img src={qrDataUrl(svg)} alt={t("remote.qrAlt")} />
        ) : (
          <div className="remote-qr-placeholder" aria-hidden />
        )}
      </div>
      <div className="remote-link-fields">
        <div className="remote-field">
          <span className="remote-field-label">{t("remote.address")}</span>
          <div className="remote-field-value">
            <code>{url}</code>
            <button
              className="icon-button"
              type="button"
              aria-label={t("action.copy")}
              onClick={copyAddress}
            >
              <Copy size={14} />
            </button>
          </div>
        </div>
        <div className="remote-field">
          <span className="remote-field-label">{t("remote.password")}</span>
          <div className="remote-field-value">
            <code>{formatRemotePassword(password)}</code>
            <button
              className="icon-button"
              type="button"
              aria-label={t("remote.passwordRotate")}
              title={t("remote.passwordRotate")}
              onClick={() => {
                run(launcherApi.remoteRotatePassword(scope));
              }}
            >
              <RefreshCw size={14} />
            </button>
            <button
              className="icon-button"
              type="button"
              aria-label={t("remote.passwordEdit")}
              title={t("remote.passwordEdit")}
              onClick={() => {
                setEditing(true);
              }}
            >
              <SquarePen size={14} />
            </button>
          </div>
        </div>
      </div>
      {editing && (
        <PasswordEditModal
          scope={scope}
          t={t}
          run={run}
          onClose={() => {
            setEditing(false);
          }}
        />
      )}
    </div>
  );
}

export function RemotePage() {
  const state = useLauncherSelector(
    (snapshot) => ({
      language: snapshot.language,
      remote: snapshot.remote,
    }),
    shallowEqual,
  );
  const { remote } = state;
  const { t } = useTranslation(undefined, { lng: state.language });
  const [disclaimerOpen, setDisclaimerOpen] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);

  const run = (task: Promise<unknown>) => {
    void task.catch((error: unknown) => {
      showTimedError(error, (key, values) => t(key, values));
    });
  };

  const closeDisclaimer = () => {
    setDisclaimerOpen(false);
    setAcknowledged(false);
  };

  const togglePublic = (enabled: boolean) => {
    if (!enabled) {
      run(launcherApi.remoteSetPublicEnabled(false, true));
      return;
    }
    run(
      launcherApi
        .remoteSetPublicEnabled(true, false)
        .catch((error: unknown) => {
          if (isErrorCode(error, "remoteDisclaimerRequired")) {
            setAcknowledged(false);
            setDisclaimerOpen(true);
            return;
          }
          throw error;
        }),
    );
  };

  const confirmPublic = () => {
    if (!acknowledged) return;
    closeDisclaimer();
    run(launcherApi.remoteSetPublicEnabled(true, true));
  };

  const publicCopy = publicStateCopy(remote.public.state);

  return (
    <section className="content-page remote-page">
      <header className="page-header">
        <h1>{t("remote.title")}</h1>
        <p>{t("remote.subtitle")}</p>
      </header>

      {!remote.serviceReady && (
        <div className="remote-notice" role="status">
          {t("remote.serviceNotReady")}
        </div>
      )}

      <section className="page-section remote-master">
        <div className="panel rows-panel">
          <div className="info-row settings-row">
            <Smartphone className="row-icon" size={18} aria-hidden />
            <div className="row-copy">
              <strong>{t("remote.master")}</strong>
              <span>{t("remote.masterDetail")}</span>
            </div>
            <SegmentedControl<"on" | "off">
              label={t("remote.master")}
              value={remote.master ? "on" : "off"}
              options={[
                { value: "on", label: t("remote.masterOn") },
                { value: "off", label: t("remote.masterOff") },
              ]}
              onChange={(choice) => {
                run(launcherApi.remoteSetMaster(choice === "on"));
              }}
            />
          </div>
        </div>
      </section>

      {!remote.master && (
        <div className="remote-notice" role="status">
          {t("remote.masterOffHint")}
        </div>
      )}

      <div
        className={
          remote.master ? "remote-sections" : "remote-sections remote-disabled"
        }
        aria-hidden={!remote.master}
      >
        <section className="page-section remote-lan">
          <h2 className="section-label">{t("remote.lan")}</h2>
          <div className="panel rows-panel">
            <div className="info-row settings-row">
              <Wifi className="row-icon" size={18} aria-hidden />
              <div className="row-copy">
                <strong>{t("remote.lan")}</strong>
                <span>{t("remote.lanDetail")}</span>
              </div>
              <SegmentedControl<"on" | "off">
                label={t("remote.lan")}
                value={remote.lan.enabled ? "on" : "off"}
                options={[
                  { value: "on", label: t("remote.masterOn") },
                  { value: "off", label: t("remote.masterOff") },
                ]}
                onChange={(choice) => {
                  run(launcherApi.remoteSetLanEnabled(choice === "on"));
                }}
              />
            </div>
            {remote.master &&
              remote.lan.enabled &&
              (remote.lan.url ? (
                <RemoteLinkPanel
                  scope="lan"
                  url={remote.lan.url}
                  password={remote.lan.password}
                  t={t}
                  run={run}
                />
              ) : (
                <div className="info-row remote-status-row">
                  <span>{t("remote.unavailable")}</span>
                </div>
              ))}
          </div>
        </section>

        <section className="page-section remote-public">
          <h2 className="section-label">{t("remote.public")}</h2>
          <div className="panel rows-panel">
            <div className="info-row settings-row">
              <Globe2 className="row-icon" size={18} aria-hidden />
              <div className="row-copy">
                <strong>{t("remote.public")}</strong>
                <span>{t("remote.publicDetail")}</span>
              </div>
              <SegmentedControl<"on" | "off">
                label={t("remote.public")}
                value={remote.public.enabled ? "on" : "off"}
                options={[
                  { value: "on", label: t("remote.masterOn") },
                  { value: "off", label: t("remote.masterOff") },
                ]}
                onChange={(choice) => {
                  togglePublic(choice === "on");
                }}
              />
            </div>
            {publicCopy && (
              <div
                className={`info-row remote-status-row ${
                  publicCopy.tone === "error" ? "remote-error-row" : ""
                }`}
                role="status"
              >
                {publicCopy.tone === "info" && (
                  <Loader2 className="spin" size={15} aria-hidden />
                )}
                <span>{t(publicCopy.key)}</span>
                {remote.public.state === "failed" && remote.public.error && (
                  <span className="proxy-error">
                    {presentError(remote.public.error, (key, values) =>
                      t(key, values),
                    )}
                  </span>
                )}
                {remote.public.state === "failed" && (
                  <button
                    className="outline-button remote-retry-button"
                    type="button"
                    onClick={() => {
                      run(launcherApi.remoteRetryPublic());
                    }}
                  >
                    <RefreshCw size={14} />
                    {t("action.retry")}
                  </button>
                )}
              </div>
            )}
            {remote.public.state === "running" &&
              (remote.public.url ? (
                <RemoteLinkPanel
                  scope="public"
                  url={remote.public.url}
                  password={remote.public.password}
                  t={t}
                  run={run}
                />
              ) : (
                <div className="info-row remote-status-row">
                  <span>{t("remote.unavailable")}</span>
                </div>
              ))}
          </div>
        </section>
      </div>

      {disclaimerOpen && (
        <div
          className="modal-overlay"
          role="presentation"
          onClick={closeDisclaimer}
        >
          <div
            className="update-modal remote-disclaimer-modal"
            role="alertdialog"
            aria-modal="true"
            aria-label={t("remote.disclaimer.title")}
            onClick={(event) => {
              event.stopPropagation();
            }}
          >
            <h2>{t("remote.disclaimer.title")}</h2>
            <p>{t("remote.disclaimer.detail")}</p>
            <label className="remote-disclaimer-check">
              <input
                type="checkbox"
                checked={acknowledged}
                onChange={(event) => {
                  setAcknowledged(event.target.checked);
                }}
              />
              <span>{t("remote.disclaimer.acknowledge")}</span>
            </label>
            <div className="modal-actions">
              <button
                className="outline-button"
                type="button"
                onClick={closeDisclaimer}
              >
                {t("action.cancel")}
              </button>
              <button
                className="primary-button"
                type="button"
                disabled={!acknowledged}
                onClick={confirmPublic}
              >
                {t("remote.disclaimer.confirm")}
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
