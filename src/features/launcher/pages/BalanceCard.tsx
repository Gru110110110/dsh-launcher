import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw, Wallet } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { BalanceSnapshot } from "@/platform/generated/bindings";
import { launcherApi } from "@/platform/launcherApi";
import { useLauncherSnapshot } from "@/platform/launcherStore";
import { showTimedError } from "@/shared/errorToast";
import { createBalancePoller } from "../balancePoller";
import { formatBalance, selectNewerBalance } from "../balancePresentation";

function errorKey(error: unknown): string {
  if (typeof error === "object" && error !== null && "code" in error) {
    const code = (error as { code?: unknown }).code;
    if (typeof code === "string") return code;
  }
  return error instanceof Error ? error.message : String(error);
}

export function BalanceCard() {
  const snapshot = useLauncherSnapshot();
  const { t } = useTranslation(undefined, { lng: snapshot.language });
  const running = snapshot.phase === "ready";
  const [balance, setBalance] = useState<BalanceSnapshot | null>(null);
  const [queried, setQueried] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const mounted = useRef(false);
  const pollerRef = useRef<ReturnType<typeof createBalancePoller> | null>(null);
  const lastAutomaticError = useRef<string | null>(null);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const notifyError = useCallback(
    (error: unknown, force: boolean) => {
      const key = errorKey(error);
      if (!force && lastAutomaticError.current === key) return;
      lastAutomaticError.current = key;
      showTimedError(error, (translationKey, values) =>
        t(translationKey, values),
      );
    },
    [t],
  );

  const accept = useCallback(
    (next: BalanceSnapshot, forceToast = false) => {
      setQueried(true);
      setBalance((current) => selectNewerBalance(current, next));
      if (next.status === "ok") {
        lastAutomaticError.current = null;
      } else {
        notifyError({ code: next.detail ?? "balanceUnavailable" }, forceToast);
      }
    },
    [notifyError],
  );

  useEffect(() => {
    if (!running) return;
    const poller = createBalancePoller({
      fetch: () => launcherApi.balanceGetSnapshot(),
      onUpdate: (next) => {
        accept(next);
      },
      onError: (error) => {
        setQueried(true);
        notifyError(error, false);
      },
    });
    pollerRef.current = poller;
    poller.start();
    return () => {
      poller.stop();
      if (pollerRef.current === poller) pollerRef.current = null;
    };
  }, [accept, notifyError, running]);

  const balanceText = balance
    ? formatBalance(balance.totalBalance, balance.currency)
    : null;

  const refresh = () => {
    if (!running || refreshing) return;
    setRefreshing(true);
    launcherApi
      .balanceRefresh()
      .then((next) => {
        if (!mounted.current) return;
        accept(next, true);
        pollerRef.current?.resetSchedule();
      })
      .catch((error: unknown) => {
        if (mounted.current) {
          setQueried(true);
          notifyError(error, true);
        }
      })
      .finally(() => {
        if (mounted.current) setRefreshing(false);
      });
  };

  return (
    <section className="page-section balance-section">
      <h2 className="section-label">{t("dashboard.balanceSection")}</h2>
      <div className="panel rows-panel">
        <div className="info-row">
          <Wallet className="row-icon" size={18} aria-hidden />
          <div className="row-copy">
            <strong>{t("balance.title")}</strong>
            <span>
              {balanceText ??
                (running && !queried
                  ? t("balance.loading")
                  : t("balance.unavailable"))}
            </span>
          </div>
          <div className="row-actions">
            <button
              type="button"
              className="inline-action"
              disabled={!running || refreshing}
              onClick={refresh}
            >
              <RefreshCw size={13} className={refreshing ? "spin" : ""} />
              {t("balance.refresh")}
            </button>
          </div>
        </div>
      </div>
    </section>
  );
}
