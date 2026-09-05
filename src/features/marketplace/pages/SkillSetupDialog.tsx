import { useEffect, useRef, useState } from "react";
import { CheckCircle2, LoaderCircle, ShieldAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { marketApi } from "@/platform/marketApi";
import { useLauncherSelector } from "@/platform/launcherStore";
import type {
  PluginSummary,
  SkillSetupStep,
} from "@/platform/generated/bindings";
import { showTimedError } from "@/shared/errorToast";
import type { Translate } from "@/shared/presentError";

import { SkillSetupGuidance } from "./SkillSetupGuidance";

export function SkillSetupDialog({
  plugin,
  steps,
  onClose,
}: {
  plugin: PluginSummary;
  steps: SkillSetupStep[];
  onClose: () => void;
}) {
  const language = useLauncherSelector((snapshot) => snapshot.language);
  const { t } = useTranslation(undefined, { lng: language });
  const closeButton = useRef<HTMLButtonElement>(null);
  const [pendingStep, setPendingStep] = useState<SkillSetupStep | null>(null);
  const [executing, setExecuting] = useState(false);
  const [output, setOutput] = useState<string | null>(null);

  useEffect(() => {
    closeButton.current?.focus();
  }, [pendingStep]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !executing) {
        if (pendingStep) setPendingStep(null);
        else onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [executing, onClose, pendingStep]);

  function execute(step: SkillSetupStep) {
    setExecuting(true);
    setOutput(null);
    const translate: Translate = (key, values) => t(key, values);
    void marketApi
      .executeSkillSetup(plugin.id, step.id)
      .then((result) => {
        if (!result.ok) return;
        setOutput(result.output);
        setPendingStep(null);
        toast.success(t("market.setup.success", { plugin: plugin.name }), {
          id: `market-setup-success-${plugin.id}`,
        });
      })
      .catch((error: unknown) => {
        showTimedError(error, translate);
      })
      .finally(() => {
        setExecuting(false);
      });
  }

  return (
    <div
      className="market-dialog-backdrop"
      role="presentation"
      onClick={() => {
        if (!executing) onClose();
      }}
    >
      <div
        className="market-dialog market-setup-dialog panel"
        role="dialog"
        aria-modal="true"
        aria-labelledby="market-setup-dialog-title"
        aria-busy={executing}
        onClick={(event) => {
          event.stopPropagation();
        }}
      >
        <header className="market-dialog-header">
          {output !== null ? (
            <CheckCircle2 size={20} aria-hidden />
          ) : (
            <ShieldAlert size={20} aria-hidden />
          )}
          <h2 id="market-setup-dialog-title">
            {t(
              pendingStep
                ? "market.setup.confirmTitle"
                : "market.setup.dialogTitle",
              { plugin: plugin.name },
            )}
          </h2>
        </header>

        {pendingStep ? (
          <>
            <p className="market-dialog-copy">
              {t("market.setup.confirmDetail")}
            </p>
            <code className="market-setup-confirm-command">
              {pendingStep.command}
            </code>
            <p className="market-dialog-copy" role="alert">
              {t("market.setup.thirdPartyWarning")}
            </p>
          </>
        ) : (
          <>
            <SkillSetupGuidance
              plugin={plugin}
              steps={steps}
              context="afterInstall"
              disabled={executing}
              onExecute={(step) => {
                setPendingStep(step);
              }}
            />
            {output !== null && (
              <section className="market-setup-output" aria-live="polite">
                <strong>{t("market.setup.output")}</strong>
                <pre>{output || t("market.setup.noOutput")}</pre>
              </section>
            )}
          </>
        )}

        <footer className="market-dialog-actions">
          <button
            ref={closeButton}
            className="outline-button"
            type="button"
            disabled={executing}
            onClick={() => {
              if (pendingStep) setPendingStep(null);
              else onClose();
            }}
          >
            {t(pendingStep ? "market.setup.back" : "market.setup.close")}
          </button>
          {pendingStep && (
            <button
              className="primary-button danger-button"
              type="button"
              disabled={executing}
              onClick={() => {
                execute(pendingStep);
              }}
            >
              {executing && <LoaderCircle size={14} className="spin" />}
              {t(
                executing
                  ? "market.setup.running"
                  : "market.setup.confirmExecute",
              )}
            </button>
          )}
        </footer>
      </div>
    </div>
  );
}
