import { useCallback, useEffect, useRef, useState } from "react";
import {
  Check,
  ExternalLink,
  LoaderCircle,
  RefreshCw,
  Search,
  Star,
  TriangleAlert,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { marketApi } from "@/platform/marketApi";
import { launcherApi } from "@/platform/launcherApi";
import { shallowEqual, useLauncherSelector } from "@/platform/launcherStore";
import type {
  MarketCatalogState,
  MarketPage,
  MarketQuery,
  MarketSort,
  PendingVerification,
  InstalledPlugin,
  PluginSummary,
} from "@/platform/generated/bindings";
import { showTimedError } from "@/shared/errorToast";
import type { Translate } from "@/shared/presentError";
import {
  INSTALLED_OPTIONS,
  KIND_OPTIONS,
  MARKET_PAGE_SIZE,
  SORT_OPTIONS,
  catalogGenerationChanged,
  compatibilityPresentation,
  formatScore,
  formatStars,
  installedFilterValue,
  isMarketCatalogUnavailable,
  marketCatalogView,
  paginationItems,
  pendingChangeLabels,
  shouldClearPendingVerification,
  type InstalledFilter,
} from "../presentation";
import { ConfirmInstallDialog } from "./ConfirmInstallDialog";
import { ConfirmUninstallDialog } from "./ConfirmUninstallDialog";

const SEARCH_DEBOUNCE_MS = 300;

function PluginCard({
  plugin,
  busy,
  disabled,
  onInstall,
  onUninstall,
}: {
  plugin: PluginSummary;
  busy: boolean;
  disabled: boolean;
  onInstall: (plugin: PluginSummary) => void;
  onUninstall: (plugin: PluginSummary) => void;
}) {
  const language = useLauncherSelector((snapshot) => snapshot.language);
  const { t } = useTranslation(undefined, { lng: language });
  const compat = compatibilityPresentation(plugin.compatibility);
  const description =
    language === "zh" ? plugin.descriptionZh : plugin.description;
  const installed = plugin.installed !== null;

  return (
    <article className="market-card panel">
      <header className="market-card-header">
        <div className="market-card-title">
          <strong>{plugin.name}</strong>
          <span className="market-card-owner">{plugin.fullName}</span>
        </div>
        <div className="market-card-badges">
          <span className={`market-kind market-kind-${plugin.kind}`}>
            {t(
              plugin.kind === "skill"
                ? "market.kind.skill"
                : "market.kind.cordisPlugin",
            )}
          </span>
          {installed && (
            <span className="market-installed-badge">
              <Check size={12} aria-hidden />
              {t("market.card.installed")}
            </span>
          )}
        </div>
      </header>

      <p className="market-card-description">{description}</p>

      {plugin.tags.length > 0 && (
        <div className="market-tags">
          {plugin.tags.slice(0, 4).map((tag) => (
            <span className="market-tag" key={tag}>
              {tag}
            </span>
          ))}
        </div>
      )}

      <div className="market-card-statuses">
        <span
          className={`market-compat market-compat-${compat.tone}`}
          title={plugin.compatibilityDetail ?? undefined}
        >
          {t(compat.labelKey)}
        </span>
        {plugin.needsConfig && (
          <span className="market-card-config">
            <TriangleAlert size={13} aria-hidden />
            {t("market.card.needsConfig")}
          </span>
        )}
      </div>

      <footer className="market-card-footer">
        <span className="market-meta" title={t("market.card.stars")}>
          <Star size={13} aria-hidden />
          {formatStars(plugin.stars)}
        </span>
        <span className="market-meta" title={t("market.card.score")}>
          <span className="market-score-label">
            {t("market.card.scoreShort")}
          </span>
          {formatScore(plugin.scoreTotal)}
        </span>
        <span className="market-card-actions">
          <button
            className="icon-button"
            type="button"
            aria-label={t("market.card.openGithub")}
            title={t("market.card.openGithub")}
            disabled={disabled}
            onClick={() => {
              void marketApi.openGithub(plugin.id).catch((error: unknown) => {
                showTimedError(error, (key, values) => t(key, values));
              });
            }}
          >
            <ExternalLink size={14} />
          </button>
          {installed ? (
            <button
              className="outline-button danger market-plugin-action-button"
              type="button"
              disabled={disabled}
              onClick={() => {
                onUninstall(plugin);
              }}
            >
              {busy && <LoaderCircle size={14} className="spin" />}
              {t("market.card.uninstall")}
            </button>
          ) : (
            <button
              className="primary-button market-plugin-action-button"
              type="button"
              disabled={disabled}
              onClick={() => {
                onInstall(plugin);
              }}
            >
              {busy && <LoaderCircle size={14} className="spin" />}
              {t("market.card.install")}
            </button>
          )}
        </span>
      </footer>
    </article>
  );
}

export function MarketplacePage() {
  const launcher = useLauncherSelector(
    (snapshot) => ({
      language: snapshot.language,
      phase: snapshot.phase,
      serviceStartedAtMs: snapshot.serviceStartedAtMs,
      marketBusy: snapshot.marketBusy,
      marketRevision: snapshot.marketRevision,
      marketCatalogRevision: snapshot.marketCatalogRevision,
    }),
    shallowEqual,
  );
  const { t } = useTranslation(undefined, { lng: launcher.language });

  const [catalog, setCatalog] = useState<MarketCatalogState | null>(null);
  const [page, setPage] = useState<MarketPage | null>(null);
  const [searchInput, setSearchInput] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [kind, setKind] = useState<"" | "cordisPlugin" | "skill">("");
  const [installedFilter, setInstalledFilter] = useState<InstalledFilter>("");
  const [sort, setSort] = useState<MarketSort>("score");
  const [pageNumber, setPageNumber] = useState(1);
  const [jumpPage, setJumpPage] = useState("");
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [busyPlugin, setBusyPlugin] = useState<string | null>(null);
  const [pending, setPending] = useState<PendingVerification | null>(null);
  const [conflict, setConflict] = useState<{
    plugin: PluginSummary;
    detail?: string;
  } | null>(null);
  const [uninstallConfirm, setUninstallConfirm] = useState<{
    plugin: PluginSummary;
    target: InstalledPlugin;
  } | null>(null);
  const dataToken = useRef(0);
  const compatToken = useRef(0);
  const refreshAttempt = useRef(0);
  const refreshRetryTimer = useRef<number | null>(null);
  const catalogStamp = useRef<string | null>(null);
  const observedMarketRevision = useRef(launcher.marketRevision);
  const observedCatalogRevision = useRef(launcher.marketCatalogRevision);
  const catalogToken = useRef(0);
  const pendingToken = useRef(0);
  const pageMounted = useRef(true);

  const translate = useCallback<Translate>(
    (key, values) => t(key, values),
    [t],
  );

  useEffect(() => {
    pageMounted.current = true;
    return () => {
      pageMounted.current = false;
    };
  }, []);

  const runPendingQuery = useCallback(() => {
    if (!pageMounted.current) return;
    const token = ++pendingToken.current;
    void marketApi
      .pendingVerification()
      .then((marker) => {
        if (pageMounted.current && token === pendingToken.current) {
          setPending(marker);
        }
      })
      .catch((error: unknown) => {
        if (pageMounted.current && token === pendingToken.current) {
          showTimedError(error, translate);
        }
      });
  }, [translate]);

  // The data query owns the page content and the loading flag; it must win
  // immediately. The compatibility pass runs independently in the background
  // and only patches badge fields when it settles.
  const runQuery = useCallback(
    (query: MarketQuery) => {
      const token = ++dataToken.current;
      marketApi
        .query(query)
        .then((result) => {
          if (token !== dataToken.current) return;
          setPage(result);
          if (result.page !== query.page) {
            setPageNumber(result.page);
          }
          setLoading(false);
        })
        .catch((error: unknown) => {
          if (token !== dataToken.current) return;
          setLoading(false);
          // Before the first catalog download finishes the query is expected
          // to fail with "not ready"; the refresh flow re-queries afterwards.
          if (isMarketCatalogUnavailable(error)) return;
          showTimedError(error, translate);
        });
    },
    [translate],
  );

  const runCompatPass = useCallback((query: MarketQuery) => {
    const token = ++compatToken.current;
    marketApi
      .query({ ...query, checkCompatibility: true })
      .then((result) => {
        if (token !== compatToken.current) return;
        setPage((previous) => {
          if (!previous || previous.page !== result.page) return previous;
          const compatById = new Map(
            result.items.map((item) => [item.id, item]),
          );
          return {
            ...previous,
            items: previous.items.map((item) => {
              const fresh = compatById.get(item.id);
              if (!fresh) return item;
              return {
                ...item,
                compatibility: fresh.compatibility,
                compatibilityDetail: fresh.compatibilityDetail,
                installVersion: fresh.installVersion,
                sourceBinding: fresh.sourceBinding,
                sourceBindingDetail: fresh.sourceBindingDetail,
              };
            }),
          };
        });
      })
      .catch(() => {
        // Badges stay "not checked"; the install flow still enforces
        // compatibility, so a silent background failure is safe.
      });
  }, []);

  // Load the current catalog state on mount. An in-flight refresh settles via
  // the durable catalog revision in the launcher snapshot, so no polling is
  // needed when this page mounts in the middle of a download.
  useEffect(() => {
    let cancelled = false;
    const token = ++catalogToken.current;

    const settle = (state: MarketCatalogState) => {
      if (state.kind === "failed") {
        refresh();
        return;
      }
      if (state.kind === "ready") {
        refreshStale(state.generatedAt);
      }
    };

    marketApi
      .catalogState()
      .then((state) => {
        if (cancelled || token !== catalogToken.current) return;
        setCatalog(state);
        settle(state);
      })
      .catch((error: unknown) => {
        if (cancelled || token !== catalogToken.current) return;
        setCatalog({ kind: "failed", message: null });
        showTimedError(error, translate);
      });
    runPendingQuery();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Clear the deferred refresh retry when the page unmounts so a retry can
  // never fire setState on an unmounted component.
  useEffect(() => {
    return () => {
      if (refreshRetryTimer.current !== null) {
        window.clearTimeout(refreshRetryTimer.current);
      }
    };
  }, []);

  // Clear only after a service start newer than the installation. Merely
  // setting `pending` while an older service is already ready must retain the
  // marker until that service has actually restarted.
  useEffect(() => {
    if (!pending) return;
    if (
      shouldClearPendingVerification(
        launcher.phase,
        launcher.serviceStartedAtMs,
        pending.installedAtMs,
      )
    ) {
      setPending(null);
    }
  }, [launcher.phase, launcher.serviceStartedAtMs, pending, translate]);

  // Track the displayed catalog generation so background refreshes can tell
  // whether the data actually changed.
  useEffect(() => {
    if (catalog?.kind === "ready") {
      catalogStamp.current = catalog.generatedAt;
    }
  }, [catalog]);

  useEffect(() => {
    const timer = setTimeout(() => {
      setAppliedSearch(searchInput.trim());
      setPageNumber(1);
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      clearTimeout(timer);
    };
  }, [searchInput]);

  const buildQuery = useCallback(
    (pageNum: number): MarketQuery => ({
      search: appliedSearch === "" ? null : appliedSearch,
      kind: kind === "" ? null : kind,
      tag: null,
      installed: installedFilterValue(installedFilter),
      sort,
      page: pageNum,
      pageSize: MARKET_PAGE_SIZE,
      checkCompatibility: false,
    }),
    [appliedSearch, kind, installedFilter, sort],
  );

  // The backend publishes a durable completion revision through the launcher
  // snapshot. A page mounted after completion gets fresh data from its initial
  // query, while a mounted page refreshes as soon as the revision changes.
  useEffect(() => {
    if (launcher.marketRevision === observedMarketRevision.current) return;
    observedMarketRevision.current = launcher.marketRevision;
    const query = buildQuery(pageNumber);
    runQuery(query);
    runCompatPass(query);
    runPendingQuery();
  }, [
    launcher.marketRevision,
    pageNumber,
    buildQuery,
    runQuery,
    runCompatPass,
    runPendingQuery,
  ]);

  useEffect(() => {
    if (launcher.marketCatalogRevision === observedCatalogRevision.current) {
      return;
    }
    observedCatalogRevision.current = launcher.marketCatalogRevision;
    const token = ++catalogToken.current;
    void marketApi
      .catalogState()
      .then((state) => {
        if (token !== catalogToken.current || !pageMounted.current) return;
        setCatalog(state);
        if (state.kind === "ready") {
          catalogStamp.current = state.generatedAt;
          const query = buildQuery(pageNumber);
          runQuery(query);
          runCompatPass(query);
        }
      })
      .catch((error: unknown) => {
        if (token === catalogToken.current && pageMounted.current) {
          showTimedError(error, translate);
        }
      });
  }, [
    launcher.marketCatalogRevision,
    pageNumber,
    buildQuery,
    runQuery,
    runCompatPass,
    translate,
  ]);

  useEffect(() => {
    setLoading(true);
    const query = buildQuery(pageNumber);
    runQuery(query);
    // A background pass fills the compatibility badges for this page only;
    // results are cached Rust-side so later pages resolve instantly.
    runCompatPass(query);
  }, [
    appliedSearch,
    kind,
    installedFilter,
    sort,
    pageNumber,
    buildQuery,
    runQuery,
    runCompatPass,
  ]);

  // Silent TTL refresh: downloads only when the cached catalog is older than
  // 24h (or missing), never blocks browsing, and stays quiet on failure.
  function refreshStale(refreshStartedAt: string | null) {
    // Keep this operation's baseline immutable. The catalog-revision effect
    // may update catalogStamp before the refresh promise resolves.
    marketApi
      .refreshCatalogIfStale()
      .then((state) => {
        // A concurrent manual refresh reports "loading" and owns the result.
        if (state.kind === "loading") return;
        if (state.kind !== "ready") {
          setCatalog(state);
          return;
        }
        const changed = catalogGenerationChanged(
          refreshStartedAt,
          state.generatedAt,
        );
        catalogStamp.current = state.generatedAt;
        if (changed || state.stale) {
          setCatalog(state);
          if (changed && state.generatedAt !== null) {
            toast.info(t("market.catalog.updated"), {
              id: "market-catalog-updated",
            });
          }
          const query = buildQuery(pageNumber);
          runQuery(query);
          runCompatPass(query);
        }
      })
      .catch(() => {
        // Background refresh stays silent; the cached catalog remains usable
        // and the manual refresh button is available for explicit retries.
      });
  }

  function refresh() {
    setRefreshing(true);
    marketApi
      .refreshCatalog()
      .then((state) => {
        // A concurrent refresh reports "loading"; leave the visible state
        // alone — the download that owns the lock will deliver the result.
        if (state.kind === "loading") return;
        setCatalog(state);
        if (state.kind === "ready") {
          refreshAttempt.current = 0;
          if (state.stale) {
            toast.error(t("market.catalog.stale"), {
              id: "market-catalog-refresh-failed",
            });
          }
          const query = buildQuery(pageNumber);
          runQuery(query);
          runCompatPass(query);
        }
      })
      .catch((error: unknown) => {
        // First downloads can fail on slow networks; retry once before
        // asking the user to intervene.
        showTimedError(error, translate);
        if (refreshAttempt.current < 1) {
          refreshAttempt.current += 1;
          refreshRetryTimer.current = window.setTimeout(() => {
            refresh();
          }, 2500);
        }
      })
      .finally(() => {
        setRefreshing(false);
      });
  }

  function prepareInstall(plugin: PluginSummary) {
    setBusyPlugin(plugin.id);
    marketApi
      .inspect(plugin.id)
      .then((inspected) => {
        setConflict({ plugin: inspected });
      })
      .catch((error: unknown) => {
        showTimedError(error, translate);
      })
      .finally(() => {
        setBusyPlugin(null);
      });
  }

  function install(plugin: PluginSummary) {
    setBusyPlugin(plugin.id);
    marketApi
      .install(plugin.id, false, plugin.installVersion)
      .then((result) => {
        if (!result.ok) {
          showTimedError(result.error, translate);
          return;
        }
        if (result.restartRequired) {
          // Keep the verification marker visible in-session: if the harness
          // fails after the restart, the pending banner must appear.
          runPendingQuery();
          toast.success(
            t("market.toast.installedRestartRequired", {
              plugin: plugin.name,
            }),
            {
              id: `market-installed-${plugin.id}`,
              duration: 12_000,
              action: {
                label: t("market.restartRequired.restart"),
                onClick: () => {
                  void launcherApi.restart().catch((error: unknown) => {
                    showTimedError(error, translate);
                  });
                },
              },
            },
          );
        } else {
          toast.success(
            t(
              plugin.kind === "skill"
                ? "market.toast.installedSkill"
                : "market.toast.installed",
            ),
            { id: `market-installed-${plugin.id}` },
          );
        }
        // Re-query to refresh installed badges.
        const query = buildQuery(pageNumber);
        runQuery(query);
      })
      .catch((error: unknown) => {
        showTimedError(error, translate);
      })
      .finally(() => {
        setBusyPlugin(null);
      });
  }

  function uninstallPlugin(pluginId: string, target: InstalledPlugin | null) {
    setBusyPlugin(pluginId);
    marketApi
      .uninstall(pluginId, target)
      .then((result) => {
        if (!result.ok) {
          showTimedError(result.error, translate);
          return;
        }
        toast.success(t("market.toast.uninstalled"), {
          id: `market-uninstalled-${pluginId}`,
        });
        if (result.restartRequired) {
          runPendingQuery();
          toast(t("market.restartRequired.detail"), {
            id: `market-restart-${pluginId}`,
            duration: 12_000,
            action: {
              label: t("market.restartRequired.restart"),
              onClick: () => {
                void launcherApi.restart().catch((error: unknown) => {
                  showTimedError(error, translate);
                });
              },
            },
          });
        }
        const query = buildQuery(pageNumber);
        runQuery(query);
      })
      .catch((error: unknown) => {
        showTimedError(error, translate);
      })
      .finally(() => {
        setBusyPlugin(null);
      });
  }

  const items = page?.items ?? [];
  const totalPages = page?.totalPages ?? 1;
  const catalogReady = catalog?.kind === "ready";
  const hasData = page !== null;
  const catalogView = marketCatalogView(catalog?.kind ?? null, hasData);
  const visiblePages = paginationItems(pageNumber, totalPages);

  function goToPage(target: number) {
    const nextPage = Math.min(totalPages, Math.max(1, Math.floor(target)));
    setPageNumber(nextPage);
  }

  function applyJump(value: string) {
    const target = Number(value);
    if (!Number.isFinite(target) || value.trim() === "") return;
    goToPage(target);
    setJumpPage("");
  }

  function submitJump(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    applyJump(jumpPage);
  }

  return (
    <section
      className="content-page market-page"
      aria-busy={launcher.marketBusy || busyPlugin !== null}
    >
      <header className="page-header">
        <h1>{t("market.title")}</h1>
        <p>{t("market.subtitle")}</p>
      </header>

      {pending && launcher.phase === "failed" && (
        <div className="market-pending panel" role="alert">
          <TriangleAlert size={16} aria-hidden />
          <span>
            {t(
              pending.journalRecovered
                ? "market.pending.recoveredDetail"
                : "market.pending.detail",
              {
                plugin: pendingChangeLabels(pending).join(", "),
              },
            )}
          </span>
          <button
            className="outline-button danger"
            type="button"
            disabled={busyPlugin !== null || launcher.marketBusy}
            onClick={() => {
              setBusyPlugin(pending.pluginId);
              void marketApi
                .rollbackPending()
                .then(() => {
                  setPending(null);
                  return launcherApi.retry();
                })
                .catch((error: unknown) => {
                  showTimedError(error, translate);
                })
                .finally(() => {
                  setBusyPlugin(null);
                });
            }}
          >
            {t("market.pending.rollback")}
          </button>
        </div>
      )}

      <div className="market-toolbar">
        <label className="market-search">
          <Search size={15} aria-hidden />
          <input
            type="search"
            value={searchInput}
            placeholder={t("market.searchPlaceholder")}
            aria-label={t("market.searchPlaceholder")}
            onChange={(event) => {
              setSearchInput(event.target.value);
            }}
          />
        </label>
        <select
          className="market-select"
          aria-label={t("market.kindLabel")}
          value={kind}
          onChange={(event) => {
            setKind(event.target.value as typeof kind);
            setPageNumber(1);
          }}
        >
          {KIND_OPTIONS.map((option) => (
            <option value={option.value} key={option.value}>
              {t(option.labelKey)}
            </option>
          ))}
        </select>
        <select
          className="market-select"
          aria-label={t("market.installedLabel")}
          value={installedFilter}
          onChange={(event) => {
            setInstalledFilter(event.target.value as typeof installedFilter);
            setPageNumber(1);
          }}
        >
          {INSTALLED_OPTIONS.map((option) => (
            <option value={option.value} key={option.value}>
              {t(option.labelKey)}
            </option>
          ))}
        </select>
        <select
          className="market-select"
          aria-label={t("market.sortLabel")}
          value={sort}
          onChange={(event) => {
            setSort(event.target.value as MarketSort);
            setPageNumber(1);
          }}
        >
          {SORT_OPTIONS.map((option) => (
            <option value={option.value} key={option.value}>
              {t(option.labelKey)}
            </option>
          ))}
        </select>
        <button
          className="market-refresh-button outline-button"
          type="button"
          disabled={refreshing}
          onClick={refresh}
        >
          <RefreshCw size={14} className={refreshing ? "spin" : ""} />
          {t("market.catalog.refresh")}
        </button>
      </div>

      {catalog !== null && catalog.kind === "ready" && hasData && (
        <p className="market-catalog-line">
          {t("market.catalog.ready", {
            count: catalog.pluginCount,
            generatedAt: catalog.generatedAt
              ? new Date(catalog.generatedAt).toLocaleDateString()
              : "–",
          })}
        </p>
      )}

      {catalogView === "loading" && (
        <div className="market-empty panel" aria-live="polite">
          <LoaderCircle size={18} className="spin" aria-hidden />
          <p>{t("market.catalog.loading")}</p>
        </div>
      )}

      {catalogView === "failed" && (
        <div className="market-empty panel">
          <p>{t("market.catalog.failed")}</p>
          <button
            className="primary-button"
            type="button"
            disabled={refreshing}
            onClick={refresh}
          >
            {t("market.catalog.retry")}
          </button>
        </div>
      )}

      {catalogView === "content" && !loading && items.length === 0 && (
        <div className="market-empty panel">
          <p>{t("market.empty")}</p>
        </div>
      )}

      {catalogView === "content" && (
        <div className={`market-grid${loading ? " market-grid-loading" : ""}`}>
          {items.map((plugin) => (
            <PluginCard
              plugin={plugin}
              key={plugin.id}
              busy={busyPlugin === plugin.id}
              disabled={busyPlugin !== null || launcher.marketBusy}
              onInstall={(target) => {
                prepareInstall(target);
              }}
              onUninstall={(target) => {
                if (target.installed !== null) {
                  setUninstallConfirm({
                    plugin: target,
                    target: target.installed,
                  });
                }
              }}
            />
          ))}
        </div>
      )}

      {catalogReady && hasData && totalPages > 1 && (
        <nav className="market-pagination" aria-label={t("market.pagination")}>
          <button
            className="market-pagination-nav outline-button"
            type="button"
            disabled={loading || pageNumber <= 1}
            onClick={() => {
              goToPage(pageNumber - 1);
            }}
          >
            {t("market.pagination.prev")}
          </button>
          <span className="market-pagination-pages">
            {visiblePages.map((item) =>
              typeof item === "number" ? (
                <button
                  className="market-page-button"
                  type="button"
                  key={item}
                  aria-label={t("market.pagination.pageLabel", { page: item })}
                  aria-current={item === pageNumber ? "page" : undefined}
                  disabled={loading}
                  onClick={() => {
                    goToPage(item);
                  }}
                >
                  {item}
                </button>
              ) : (
                <span className="market-pagination-ellipsis" key={item}>
                  …
                </span>
              ),
            )}
          </span>
          <button
            className="market-pagination-nav outline-button"
            type="button"
            disabled={loading || pageNumber >= totalPages}
            onClick={() => {
              goToPage(pageNumber + 1);
            }}
          >
            {t("market.pagination.next")}
          </button>
          <form className="market-pagination-jump" onSubmit={submitJump}>
            <label htmlFor="market-jump-page">
              {t("market.pagination.jumpTo")}
            </label>
            <input
              id="market-jump-page"
              type="number"
              min={1}
              max={totalPages}
              inputMode="numeric"
              value={jumpPage}
              disabled={loading}
              aria-label={t("market.pagination.jumpLabel", {
                total: totalPages,
              })}
              onChange={(event) => {
                setJumpPage(event.target.value);
              }}
              onKeyDown={(event) => {
                if (event.key !== "Enter") return;
                event.preventDefault();
                applyJump(event.currentTarget.value);
              }}
            />
            <span>{t("market.pagination.pageUnit")}</span>
            <button
              className="outline-button"
              type="submit"
              disabled={loading || jumpPage.trim() === ""}
            >
              {t("market.pagination.go")}
            </button>
          </form>
        </nav>
      )}

      {conflict !== null && (
        <ConfirmInstallDialog
          plugin={conflict.plugin}
          detail={conflict.detail}
          disabled={launcher.marketBusy || busyPlugin !== null}
          onCancel={() => {
            setConflict(null);
          }}
          onConfirm={() => {
            const plugin = conflict.plugin;
            setConflict(null);
            install(plugin);
          }}
        />
      )}
      {uninstallConfirm !== null && (
        <ConfirmUninstallDialog
          plugin={uninstallConfirm.plugin}
          target={uninstallConfirm.target}
          disabled={launcher.marketBusy || busyPlugin !== null}
          onCancel={() => {
            setUninstallConfirm(null);
          }}
          onConfirm={() => {
            const { plugin, target } = uninstallConfirm;
            setUninstallConfirm(null);
            uninstallPlugin(plugin.id, target);
          }}
        />
      )}
    </section>
  );
}
