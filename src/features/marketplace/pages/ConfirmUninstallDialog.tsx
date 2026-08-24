import { useEffect, useRef } from "react";
import { TriangleAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useLauncherSnapshot } from "@/platform/launcherStore";
import type {
  InstalledPlugin,
  PluginSummary,
} from "@/platform/generated/bindings";

export function ConfirmUninstallDialog({
  plugin,
  target,
  onCancel,
  onConfirm,
}: {
  plugin: PluginSummary;
  target: InstalledPlugin;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const snapshot = useLauncherSnapshot();
  const { t } = useTranslation(undefined, { lng: snapshot.language });
  const cancelButton = useRef<HTMLButtonElement>(null);
  const location =
    target.source === "profile"
      ? t("market.uninstall.locationProfile", {
          profile: target.profile ?? "web",
        })
      : t("market.uninstall.locationSkill", { name: target.localName });

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
        aria-labelledby="market-uninstall-dialog-title"
        onClick={(event) => {
          event.stopPropagation();
        }}
      >
        <header className="market-dialog-header">
          <TriangleAlert size={20} aria-hidden />
          <h2 id="market-uninstall-dialog-title">
            {t("market.uninstall.title", { plugin: plugin.name })}
          </h2>
        </header>
        <p className="market-dialog-copy">{t("market.uninstall.detail")}</p>
        <p className="market-dialog-detail">{location}</p>
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
            onClick={onConfirm}
          >
            {t("market.card.uninstall")}
          </button>
        </footer>
      </div>
    </div>
  );
}
