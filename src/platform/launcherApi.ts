import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  BalanceSnapshot,
  HarnessUpdateChannel,
  HarnessUpdateMode,
  Language,
  LauncherSnapshot,
  ProxySettings,
  ProxyTestReport,
  RemoteScope,
  ThemePreference,
  StartupRepairBackupSummary,
} from "./generated/bindings";

const command = <T>(name: string, args?: Record<string, unknown>): Promise<T> =>
  invoke<T>(name, args);
const action = (name: string, args?: Record<string, unknown>): Promise<void> =>
  invoke(name, args);

export const launcherApi = {
  snapshot: () => command<LauncherSnapshot>("launcher_get_snapshot"),
  retry: () => action("launcher_retry"),
  rollbackHarness: (expectedVersion: string) =>
    command<string>("launcher_rollback_harness", { expectedVersion }),
  repairAndStart: () => action("launcher_repair_and_start"),
  acknowledgeStartupRepair: () => action("launcher_acknowledge_startup_repair"),
  startupRepairBackups: () =>
    command<StartupRepairBackupSummary>("launcher_startup_repair_backups"),
  clearStartupRepairBackups: () =>
    command<StartupRepairBackupSummary>(
      "launcher_clear_startup_repair_backups",
    ),
  stop: () => action("launcher_stop"),
  restart: () => action("launcher_restart"),
  checkHarnessUpdate: () =>
    command<string | null>("launcher_check_harness_update"),
  updateHarness: (mode: HarnessUpdateMode, expectedVersion: string) =>
    action("launcher_update_harness", { mode, expectedVersion }),
  activateHarnessUpdate: (expectedVersion: string) =>
    action("launcher_activate_harness_update", { expectedVersion }),
  approveMigration: () => action("migration_approve"),
  skipMigration: () => action("migration_skip"),
  selectBrowser: (browserId: string) =>
    action("launcher_select_browser", { browserId }),
  openWebUi: () => action("launcher_open_web_ui"),
  openWebsite: () => action("application_open_website"),
  openExternalLink: (target: "github" | "deepseek" | "harnessGithub") =>
    action("application_open_external_link", { target }),
  copyWebUrl: () => action("application_copy_web_url"),
  setLanguage: (language: Language) =>
    action("preferences_set_language", { language }),
  setTheme: (theme: ThemePreference) =>
    action("preferences_set_theme", { theme }),
  setShowBalanceCard: (show: boolean) =>
    action("preferences_set_show_balance_card", { show }),
  setHarnessUpdateChannel: (channel: HarnessUpdateChannel) =>
    action("preferences_set_harness_update_channel", { channel }),
  setProxy: (proxy: ProxySettings) =>
    command<boolean>("preferences_set_proxy", { proxy }),
  testProxy: (proxy: ProxySettings) =>
    command<ProxyTestReport>("proxy_test_connection", { proxy }),
  balanceGetSnapshot: () => command<BalanceSnapshot>("balance_get_snapshot"),
  balanceRefresh: () => command<BalanceSnapshot>("balance_refresh"),
  checkDesktopUpdate: () => command<string | null>("application_check_update"),
  installDesktopUpdate: () => action("application_install_update"),
  remoteSetMaster: (enabled: boolean) =>
    action("remote_set_master", { enabled }),
  remoteSetLanEnabled: (enabled: boolean) =>
    action("remote_set_lan_enabled", { enabled }),
  remoteRefreshLan: () => action("remote_refresh_lan"),
  remoteSetPublicEnabled: (enabled: boolean, acknowledged: boolean) =>
    action("remote_set_public_enabled", { enabled, acknowledged }),
  remoteRotatePassword: (scope: RemoteScope) =>
    action("remote_rotate_password", { scope }),
  remoteSetPassword: (scope: RemoteScope, password: string) =>
    action("remote_set_password", { scope, password }),
  remoteRetryPublic: () => action("remote_retry_public"),
  remoteQr: (scope: RemoteScope) => command<string>("remote_qr", { scope }),
  onState: (
    handler: (snapshot: LauncherSnapshot) => void,
  ): Promise<UnlistenFn> =>
    listen<LauncherSnapshot>("launcher://state", ({ payload }) => {
      handler(payload);
    }),
};
