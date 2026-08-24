import { invoke } from "@tauri-apps/api/core";
import type {
  InstalledPlugin,
  MarketCatalogState,
  MarketOperationResult,
  MarketPage,
  MarketQuery,
  PendingVerification,
  PluginKind,
  PluginSummary,
} from "./generated/bindings";

const isTauri = "__TAURI_INTERNALS__" in window;

const command = <T>(name: string, args?: Record<string, unknown>): Promise<T> =>
  invoke<T>(name, args);
const action = (name: string, args?: Record<string, unknown>): Promise<void> =>
  invoke(name, args);

export const marketApi = {
  catalogState: () => command<MarketCatalogState>("market_get_catalog"),
  refreshCatalog: () => command<MarketCatalogState>("market_refresh_catalog"),
  refreshCatalogIfStale: () =>
    command<MarketCatalogState>("market_refresh_if_stale"),
  query: (query: MarketQuery) => command<MarketPage>("market_query", { query }),
  installed: () => command<InstalledPlugin[]>("market_installed"),
  inspect: (pluginId: string) =>
    command<PluginSummary>("market_inspect", { pluginId }),
  install: (pluginId: string, force: boolean, expectedVersion: string | null) =>
    command<MarketOperationResult>("market_install", {
      pluginId,
      force,
      expectedVersion,
    }),
  uninstall: (pluginId: string, target: InstalledPlugin | null) =>
    command<MarketOperationResult>("market_uninstall", { pluginId, target }),
  pendingVerification: () =>
    command<PendingVerification | null>("market_pending_verification"),
  rollbackPending: () => action("market_rollback_pending"),
  openGithub: (pluginId: string) =>
    action("market_open_plugin_github", { pluginId }),
};

// ---------------------------------------------------------------------------
// Browser development preview: a small in-memory catalog so the page can be
// iterated with `pnpm dev` outside the Tauri runtime. Never used in the app.
// ---------------------------------------------------------------------------

type DevKind = Extract<PluginKind, "cordisPlugin" | "skill">;

const devPlugins: PluginSummary[] = [
  devPlugin(
    "2BingLing/dsh-market",
    "cordisPlugin",
    "dsh-market",
    61,
    88,
    "DSH 插件市场：中文搜索、五维评分、一键安装。",
    ["marketplace", "搜索", "评分"],
  ),
  devPlugin(
    "example/web-search",
    "cordisPlugin",
    "web-search",
    421,
    76,
    "Bring live web search into the conversation.",
    ["web-search", "search"],
  ),
  devPlugin(
    "example/pdf-reader",
    "skill",
    "pdf-reader",
    233,
    61,
    "Read and summarize PDF files through a skill file.",
    ["pdf", "文档"],
  ),
  devPlugin(
    "example/code-review",
    "cordisPlugin",
    "code-review",
    158,
    52,
    "Review staged diffs before commit.",
    ["git", "review"],
  ),
  devPlugin(
    "example/chart-render",
    "skill",
    "chart-render",
    97,
    40,
    "Render charts from conversation data.",
    ["chart", "可视化"],
  ),
  devPlugin(
    "example/i18n-helper",
    "cordisPlugin",
    "i18n-helper",
    54,
    33,
    "Translate UI dictionaries in bulk.",
    ["i18n", "翻译"],
  ),
];

// One installed entry so the install-state filter is demonstrable in the
// browser dev preview.
const firstDevPlugin = devPlugins[0];
if (firstDevPlugin !== undefined) {
  devPlugins[0] = {
    ...firstDevPlugin,
    installed: {
      pluginId: firstDevPlugin.id,
      localName: "dsh-market",
      version: "0.3.1",
      source: "profile",
      profile: "web",
    },
  };
}

function devPlugin(
  id: string,
  kind: DevKind,
  name: string,
  stars: number,
  score: number,
  descriptionZh: string,
  tags: string[],
): PluginSummary {
  return {
    id,
    kind,
    name,
    owner: id.split("/")[0] ?? "",
    repo: name,
    fullName: id,
    stars,
    description: descriptionZh,
    descriptionZh,
    tags,
    homepage:
      kind === "cordisPlugin" ? `https://www.npmjs.com/package/${name}` : null,
    license: "MIT",
    curated: false,
    pushedAt: null,
    updatedAt: "2026-08-20T00:00:00Z",
    needsConfig: false,
    installMethod: kind === "cordisPlugin" ? "pnpm-profile" : "skills-add",
    installTarget: kind === "cordisPlugin" ? name : id,
    installVersion:
      kind === "cordisPlugin"
        ? "1.0.0"
        : "1111111111111111111111111111111111111111",
    sourceBinding: "verified",
    sourceBindingDetail: null,
    scoreTotal: score,
    scoreExplanation: "维护活跃，安装便捷。",
    compatibility: "notChecked",
    compatibilityDetail: null,
    installed: null,
  };
}

const delay = <T>(value: T, ms = 180): Promise<T> =>
  new Promise((resolve) => {
    setTimeout(() => {
      resolve(value);
    }, ms);
  });

const devQuery = (query: MarketQuery): Promise<MarketPage> => {
  const kind = query.kind;
  const search = (query.search ?? "").toLowerCase();
  const matched = devPlugins.filter((plugin) => {
    if (kind && plugin.kind !== kind) return false;
    if (query.installed !== null) {
      const isInstalled = plugin.installed !== null;
      if (isInstalled !== query.installed) return false;
    }
    if (!search) return true;
    const haystack =
      `${plugin.name} ${plugin.description} ${plugin.descriptionZh} ${plugin.tags.join(" ")}`.toLowerCase();
    return search.split(/\s+/).every((term) => haystack.includes(term));
  });
  const pageSize = Math.max(1, query.pageSize);
  const totalPages = Math.max(1, Math.ceil(matched.length / pageSize));
  const page = Math.min(Math.max(1, query.page), totalPages);
  const items = matched.slice((page - 1) * pageSize, page * pageSize);
  return delay({
    items: items.map((plugin) => ({
      ...plugin,
      compatibility: query.checkCompatibility
        ? "compatible"
        : plugin.compatibility,
      compatibilityDetail: query.checkCompatibility ? "cordis 4.0.1" : null,
    })),
    total: matched.length,
    page,
    pageSize,
    totalPages,
    generatedAt: "2026-08-23T05:30:24.636Z",
  });
};

if (!isTauri) {
  marketApi.catalogState = () =>
    delay<MarketCatalogState>({
      kind: "ready",
      generatedAt: "2026-08-23T05:30:24.636Z",
      pluginCount: devPlugins.length,
      stale: false,
    });
  marketApi.refreshCatalog = () =>
    delay<MarketCatalogState>({
      kind: "ready",
      generatedAt: "2026-08-23T05:30:24.636Z",
      pluginCount: devPlugins.length,
      stale: false,
    });
  marketApi.refreshCatalogIfStale = () =>
    delay<MarketCatalogState>({
      kind: "ready",
      generatedAt: "2026-08-23T05:30:24.636Z",
      pluginCount: devPlugins.length,
      stale: false,
    });
  marketApi.inspect = (pluginId) => {
    const plugin = devPlugins.find((candidate) => candidate.id === pluginId);
    return plugin
      ? delay({ ...plugin })
      : Promise.reject(new Error(`plugin not found: ${pluginId}`));
  };
  marketApi.query = devQuery;
  marketApi.installed = () => delay<InstalledPlugin[]>([]);
  marketApi.install = (pluginId: string) =>
    delay<MarketOperationResult>(
      {
        ok: true,
        action: "install",
        pluginId,
        restartRequired: false,
        error: null,
      },
      800,
    );
  marketApi.uninstall = (pluginId: string) =>
    delay<MarketOperationResult>(
      {
        ok: true,
        action: "uninstall",
        pluginId,
        restartRequired: false,
        error: null,
      },
      600,
    );
  marketApi.pendingVerification = () => delay<PendingVerification | null>(null);
  marketApi.rollbackPending = () => Promise.resolve();
  marketApi.openGithub = () => Promise.resolve();
}
