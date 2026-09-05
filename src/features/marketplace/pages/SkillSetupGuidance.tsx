import { Copy, ExternalLink, Play, ShieldAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { marketApi } from "@/platform/marketApi";
import { useLauncherSelector } from "@/platform/launcherStore";
import type {
  PluginSummary,
  SkillSetupStep,
} from "@/platform/generated/bindings";
import { showTimedError } from "@/shared/errorToast";

export function SkillSetupGuidance({
  plugin,
  steps = plugin.setupSteps,
  context,
  disabled = false,
  onExecute,
}: {
  plugin: PluginSummary;
  steps?: SkillSetupStep[];
  context: "beforeInstall" | "afterInstall";
  disabled?: boolean;
  onExecute?: (step: SkillSetupStep) => void;
}) {
  const language = useLauncherSelector((snapshot) => snapshot.language);
  const { t } = useTranslation(undefined, { lng: language });

  function copyCommand(step: SkillSetupStep) {
    void navigator.clipboard
      .writeText(step.command)
      .then(() => {
        toast.success(t("market.setup.copied"), {
          id: `market-setup-copied-${step.id}`,
        });
      })
      .catch(() => {
        toast.error(t("market.setup.copyFailed"));
      });
  }

  return (
    <section
      className="market-setup-guidance"
      aria-label={t("market.setup.title")}
    >
      <header className="market-setup-guidance-header">
        <ShieldAlert size={16} aria-hidden />
        <strong>{t("market.setup.title")}</strong>
      </header>
      <p className="market-setup-guidance-copy">
        {t(`market.setup.${context}`)}
      </p>
      {steps.length === 0 ? (
        <p className="market-setup-guidance-copy">
          {t("market.setup.missingCommand")}
        </p>
      ) : (
        <ul className="market-setup-list">
          {steps.map((step) => (
            <li key={step.id}>
              <code>{step.command}</code>
              <div className="market-setup-step-actions">
                <button
                  className="outline-button"
                  type="button"
                  disabled={disabled}
                  aria-label={t("market.setup.copyCommand", {
                    command: step.command,
                  })}
                  onClick={() => {
                    copyCommand(step);
                  }}
                >
                  <Copy size={13} aria-hidden />
                  {t("market.setup.copy")}
                </button>
                {step.canExecute && onExecute ? (
                  <button
                    className="primary-button"
                    type="button"
                    disabled={disabled}
                    onClick={() => {
                      onExecute(step);
                    }}
                  >
                    <Play size={13} aria-hidden />
                    {t("market.setup.execute")}
                  </button>
                ) : !step.canExecute ? (
                  <span className="market-setup-copy-only">
                    {t("market.setup.copyOnly")}
                  </span>
                ) : null}
              </div>
            </li>
          ))}
        </ul>
      )}
      <button
        className="market-setup-docs-button"
        type="button"
        disabled={disabled}
        onClick={() => {
          void marketApi.openGithub(plugin.id).catch((error: unknown) => {
            showTimedError(error, (key, values) => t(key, values));
          });
        }}
      >
        <ExternalLink size={13} aria-hidden />
        {t("market.setup.openDocs")}
      </button>
    </section>
  );
}
