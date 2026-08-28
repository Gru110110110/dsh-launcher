import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  BalanceSnapshot,
  HarnessUpdateMode,
  Language,
  LauncherSnapshot,
  ProxySettings,
  ProxyTestReport,
  ThemePreference,
} from "./generated/bindings";

const command = <T>(name: string, args?: Record<string, unknown>): Promise<T> =>
  invoke<T>(name, args);
const action = (name: string, args?: Record<string, unknown>): Promise<void> =>
  invoke(name, args);

export const launcherApi = {
  snapshot: () => command<LauncherSnapshot>("launcher_get_snapshot"),
  retry: () => action("launcher_retry"),
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
  setProxy: (proxy: ProxySettings) =>
    command<boolean>("preferences_set_proxy", { proxy }),
  testProxy: (proxy: ProxySettings) =>
    command<ProxyTestReport>("proxy_test_connection", { proxy }),
  balanceGetSnapshot: () => command<BalanceSnapshot>("balance_get_snapshot"),
  balanceRefresh: () => command<BalanceSnapshot>("balance_refresh"),
  checkDesktopUpdate: () => command<string | null>("application_check_update"),
  installDesktopUpdate: () => action("application_install_update"),
  onState: (
    handler: (snapshot: LauncherSnapshot) => void,
  ): Promise<UnlistenFn> =>
    listen<LauncherSnapshot>("launcher://state", ({ payload }) => {
      handler(payload);
    }),
};
