// Generated from dsh-core Rust types. Do not edit by hand.

export type Language = "zh" | "en";

export type ThemePreference = "system" | "light" | "dark";

export type LauncherPhase =
  | "preparing"
  | "awaitingMigration"
  | "starting"
  | "ready"
  | "stopped"
  | "failed"
  | "stopping";

export type LauncherStep = "prepare" | "start";

export type BrowserChoice = { id: string; label: string };

export type ActivityCode =
  | "waitingForLock"
  | "checkingRuntime"
  | "resolvingVersion"
  | "downloadingNode"
  | "verifyingNode"
  | "checkingSources"
  | "copyingHarnessRuntime"
  | "installingHarness"
  | "resolvingHarnessDependencies"
  | "downloadingHarnessPackages"
  | "writingHarnessRuntime"
  | "validatingHarness"
  | "activatingHarness"
  | "migratingData"
  | "startingService";

export type ActivityState = {
  code: ActivityCode;
  values: { [key in string]?: string };
  startedAtMs: number;
};

export type ProgressState =
  | { kind: "indeterminate" }
  | { kind: "determinate"; done: number; total: number };

export type LauncherError = {
  code: string;
  values: { [key in string]?: string };
  safeDetail?: string | null;
};

export type DesktopUpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "available"; version: string }
  | { kind: "preparing"; version: string }
  | { kind: "downloading"; version: string; done: number; total: number | null }
  | { kind: "installing"; version: string }
  | { kind: "failed"; version: string | null };

export type HarnessUpdateState =
  | { kind: "none" }
  | { kind: "checking" }
  | { kind: "available"; version: string }
  | { kind: "downloading"; version: string }
  | { kind: "downloaded"; version: string }
  | { kind: "installing"; version: string }
  | { kind: "failed"; version: string };

export type HarnessUpdateMode = "foreground" | "background";

export type MigrationPlan = {
  sourceEntries: number;
  workspaceAvailable: boolean;
  ccSwitchProviders: number;
};

export type MigrationState =
  | { kind: "notRequired" }
  | { kind: "pending"; plan: MigrationPlan }
  | { kind: "applying"; plan: MigrationPlan }
  | { kind: "completed" }
  | { kind: "completedWithWarning"; warning: LauncherError }
  | { kind: "skipped" };

export type LauncherSnapshot = {
  revision: number;
  marketBusy: boolean;
  marketRevision: number;
  marketCatalogRevision: number;
  phase: LauncherPhase;
  step: LauncherStep;
  activity: ActivityState | null;
  progress: ProgressState;
  error: LauncherError | null;
  webUrl: string | null;
  serviceStartedAtMs: number | null;
  browsers: Array<BrowserChoice>;
  selectedBrowserId: string;
  language: Language;
  theme: ThemePreference;
  desktopVersion: string;
  harnessVersion: string | null;
  desktopUpdate: DesktopUpdateState;
  harnessUpdate: HarnessUpdateState;
  migration: MigrationState;
  trayAvailable: boolean;
  showBalanceCard: boolean;
  proxy: ProxySettings;
};

export type PluginKind = "cordisPlugin" | "skill";

export type PluginSource = "skills" | "profile";

export type CompatibilityStatus =
  | "notChecked"
  | "compatible"
  | "incompatible"
  | "unknown";

export type SourceBindingStatus =
  | "notChecked"
  | "verified"
  | "mismatch"
  | "unknown";

export type CompatibilityInfo = {
  status: CompatibilityStatus;
  detail: string | null;
};

export type InstalledPlugin = {
  pluginId: string | null;
  localName: string;
  version: string | null;
  source: PluginSource;
  profile: string | null;
};

export type PluginSummary = {
  id: string;
  kind: PluginKind;
  name: string;
  owner: string;
  repo: string;
  fullName: string;
  stars: number;
  description: string;
  descriptionZh: string;
  tags: Array<string>;
  homepage: string | null;
  license: string | null;
  curated: boolean;
  pushedAt: string | null;
  updatedAt: string | null;
  needsConfig: boolean;
  installMethod: string;
  installTarget: string;
  installVersion: string | null;
  sourceBinding: SourceBindingStatus;
  sourceBindingDetail: string | null;
  scoreTotal: number | null;
  scoreExplanation: string | null;
  compatibility: CompatibilityStatus;
  compatibilityDetail: string | null;
  installed: InstalledPlugin | null;
};

export type PluginCompatibility = {
  pluginId: string;
  compatibility: CompatibilityStatus;
  compatibilityDetail: string | null;
  installVersion: string | null;
  sourceBinding: SourceBindingStatus;
  sourceBindingDetail: string | null;
};

export type MarketSort = "score" | "stars" | "recentlyUpdated" | "name";

export type MarketQuery = {
  search: string | null;
  kind: PluginKind | null;
  tag: string | null;
  /**
   * Install-state filter: `None` = all, `Some(true)` = installed only,
   * `Some(false)` = not installed only.
   */
  installed: boolean | null;
  sort: MarketSort;
  page: number;
  pageSize: number;
  checkCompatibility: boolean;
};

export type MarketPage = {
  items: Array<PluginSummary>;
  total: number;
  page: number;
  pageSize: number;
  totalPages: number;
  generatedAt: string | null;
};

export type MarketCatalogState =
  | { kind: "loading" }
  | {
      kind: "ready";
      generatedAt: string | null;
      pluginCount: number;
      stale: boolean;
    }
  | { kind: "failed"; message: string | null };

export type MarketOperationKind = "install" | "uninstall";

export type MarketOperationResult = {
  ok: boolean;
  action: MarketOperationKind;
  pluginId: string;
  restartRequired: boolean;
  error: LauncherError | null;
};

export type PendingMarketChange = {
  pluginId: string;
  name: string;
  action: MarketOperationKind;
  profile: string | null;
};

export type PendingVerification = {
  /**
   * The most recent change is retained in the legacy fields so pending
   * markers written by older launchers remain readable after an update.
   */
  pluginId: string;
  name: string;
  installedAtMs: number;
  changes: Array<PendingMarketChange>;
  /**
   * The original journal was unreadable and was quarantined. The profile
   * names in `changes` remain sufficient for a safe rollback, but the
   * individual plugin identities are no longer trustworthy.
   */
  journalRecovered: boolean;
};

export type BalanceStatus = "ok" | "stale" | "unavailable";

export type BalanceSnapshot = {
  status: BalanceStatus;
  detail: string | null;
  isAvailable: boolean | null;
  currency: string | null;
  totalBalance: string | null;
  fetchedAtMs: number | null;
};

export type ProxyMode = "system" | "direct" | "manual";

export type ProxySettings = {
  mode: ProxyMode;
  /**
   * Single proxy URL for manual mode (http, https, socks5, socks5h).
   */
  url: string;
  /**
   * Optional comma/semicolon separated bypass (NO_PROXY) list for manual
   * mode.
   */
  bypass: string;
};

export type NetworkErrorKind =
  | "timeout"
  | "proxyAuth"
  | "tls"
  | "connect"
  | "httpStatus"
  | "other";

export type ProxyTestSource = { source: string; version: string };

export type ProxyTestFailure = {
  source: string;
  kind: NetworkErrorKind;
  detail: string;
};

export type ProxyTestReport = {
  sources: Array<ProxyTestSource>;
  failures: Array<ProxyTestFailure>;
};
