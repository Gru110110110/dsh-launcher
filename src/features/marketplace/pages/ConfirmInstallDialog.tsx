import { useEffect, useRef, useState } from "react";
import { TriangleAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useLauncherSelector } from "@/platform/launcherStore";
import type { PluginSummary } from "@/platform/generated/bindings";

const BINDING_TRANSLATION_KEYS = {
  notChecked: "market.install.binding.notChecked",
  verified: "market.install.binding.verified",
  mismatch: "market.install.binding.mismatch",
  unknown: "market.install.binding.unknown",
} as const satisfies Record<PluginSummary["sourceBinding"], string>;

export function ConfirmInstallDialog({
  plugin,
  detail,
  disabled = false,
  onCancel,
  onConfirm,
}: {
  plugin: PluginSummary;
  detail?: string;
  disabled?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const language = useLauncherSelector((snapshot) => snapshot.language);
  const { t } = useTranslation(undefined, { lng: language });
  const cancelButton = useRef<HTMLButtonElement>(null);
  const submitted = useRef(false);
  const [submitting, setSubmitting] = useState(false);

  const confirmOnce = () => {
    if (disabled || submitted.current) return;
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
        <p className="market-dialog-copy">{t("market.install.detail")}</p>
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
          {t("market.install.target", { target: plugin.installTarget })}
          <br />
          {t("market.install.version", {
            version:
              plugin.installVersion ?? t("market.install.versionPending"),
          })}
          <br />
          {t(
            plugin.kind === "skill"
              ? "market.install.binding.skill"
              : BINDING_TRANSLATION_KEYS[plugin.sourceBinding],
          )}
        </p>
        {detail !== undefined && detail.length > 0 && (
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
            disabled={disabled || submitting}
            onClick={confirmOnce}
          >
            {t("market.install.confirm")}
          </button>
        </footer>
      </div>
    </div>
  );
}
