import { useEffect, useRef, useState } from "react";
import { TriangleAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useLauncherSelector } from "@/platform/launcherStore";
import type { PluginSummary } from "@/platform/generated/bindings";

import { installReviewState } from "../presentation";

const BINDING_TRANSLATION_KEYS = {
  notChecked: "market.install.binding.notChecked",
  verified: "market.install.binding.verified",
  mismatch: "market.install.binding.mismatch",
  unknown: "market.install.binding.unknown",
} as const satisfies Record<PluginSummary["sourceBinding"], string>;

export function ConfirmInstallDialog({
  plugin,
  detail,
  risky = false,
  disabled = false,
  onCancel,
  onConfirm,
}: {
  plugin: PluginSummary;
  detail?: string;
  risky?: boolean;
  disabled?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const language = useLauncherSelector((snapshot) => snapshot.language);
  const { t } = useTranslation(undefined, { lng: language });
  const cancelButton = useRef<HTMLButtonElement>(null);
  const submitted = useRef(false);
  const [submitting, setSubmitting] = useState(false);

  const confirmDisabled = disabled || installReviewState(plugin) === "blocked";

  const confirmOnce = () => {
    if (confirmDisabled || submitted.current) return;
    submitted.current = true;
    setSubmitting(true);
    onConfirm();
  };

  useEffect(() => {
    cancelButton.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onCancel();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [onCancel]);

  return (
    <div
      className="market-dialog-backdrop"
      role="presentation"
      onClick={onCancel}
    >
      <div
        className="market-dialog panel"
        role="dialog"
        aria-modal="true"
        aria-labelledby="market-dialog-title"
        onClick={(event) => {
          event.stopPropagation();
        }}
      >
        <header className="market-dialog-header">
          <TriangleAlert size={20} aria-hidden />
          <h2 id="market-dialog-title">
            {t("market.install.title", { plugin: plugin.name })}
          </h2>
        </header>
        <p className="market-dialog-copy">
          {t(risky ? "market.confirm.detail" : "market.install.detail")}
        </p>
        <p className="market-dialog-copy">
          {t(
            plugin.kind === "skill"
              ? "market.install.riskSkill"
              : "market.install.riskCordis",
          )}
        </p>
        <p className="market-dialog-detail">
          {t("market.install.source", { source: plugin.id })}
          <br />
          {plugin.kind === "cordisPlugin" ? (
            <>
              {t("market.install.profile", { profile: plugin.installProfile })}
              <br />
              {t("market.install.target", {
                target: plugin.installPackages.join(", "),
              })}
              <br />
              {plugin.installProfile !== "web" && (
                <>
                  {t("market.install.profileLaunch", {
                    profile: plugin.installProfile,
                  })}
                  <br />
                </>
              )}
            </>
          ) : (
            <>
              {t("market.install.target", { target: plugin.installTarget })}
              <br />
              {t("market.install.version", {
                version:
                  plugin.installVersion ?? t("market.install.versionPending"),
              })}
              <br />
            </>
          )}
          {t(
            plugin.kind === "skill"
              ? "market.install.binding.skill"
              : BINDING_TRANSLATION_KEYS[plugin.sourceBinding],
          )}
        </p>
        {installReviewState(plugin) === "blocked" && (
          <p className="market-dialog-detail" role="alert">
            {t("market.install.unresolved")}
          </p>
        )}
        {plugin.kind === "cordisPlugin" && plugin.sourceBindingDetail && (
          <p className="market-dialog-detail" role="alert">
            {plugin.sourceBindingDetail}
          </p>
        )}
        {plugin.kind === "cordisPlugin" &&
          plugin.compatibility !== "compatible" && (
            <p className="market-dialog-detail">
              {t("market.confirm.compatibility")}
              {plugin.compatibilityDetail && <> {plugin.compatibilityDetail}</>}
            </p>
          )}
        {detail !== undefined &&
          detail.length > 0 &&
          detail !== plugin.sourceBindingDetail &&
          detail !== plugin.compatibilityDetail && (
            <p className="market-dialog-detail">{detail}</p>
          )}
        <footer className="market-dialog-actions">
          <button
            ref={cancelButton}
            className="outline-button"
            type="button"
            onClick={onCancel}
          >
            {t("action.cancel")}
          </button>
          <button
            className="primary-button danger-button"
            type="button"
            disabled={confirmDisabled || submitting}
            onClick={confirmOnce}
          >
            {t(
              risky ? "market.confirm.installAnyway" : "market.install.confirm",
            )}
          </button>
        </footer>
      </div>
    </div>
  );
}
