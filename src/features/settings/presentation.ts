import type {
  DesktopUpdateState,
  ProxyMode,
  ProxySettings,
  ProxyTestReport,
} from "@/platform/generated/bindings";

type CopySpec = {
  key: string;
  values?: Record<string, string | number>;
};

type DesktopUpdateAction = {
  appearance: "default" | "primary";
  disabled: boolean;
  spinning: boolean;
  operation: "check" | "install" | null;
  label: CopySpec;
};

function downloadPercent(
  desktopUpdate: Extract<DesktopUpdateState, { kind: "downloading" }>,
): number | null {
  const total = desktopUpdate.total;
  if (total === null || total <= 0) return null;
  return Math.min(100, Math.floor((desktopUpdate.done * 100) / total));
}

export function getDesktopUpdateDetail(
  desktopUpdate: DesktopUpdateState,
): CopySpec {
  switch (desktopUpdate.kind) {
    case "checking":
      return { key: "update.desktop.checking" };
    case "available":
      return {
        key: "update.desktop.available",
        values: { version: desktopUpdate.version },
      };
    case "preparing":
      return {
        key: "update.desktop.preparing",
        values: { version: desktopUpdate.version },
      };
    case "downloading": {
      const percent = downloadPercent(desktopUpdate);
      return {
        key:
          percent !== null
            ? "update.desktop.downloading"
            : "update.desktop.downloadingUnknown",
        values:
          percent !== null
            ? { version: desktopUpdate.version, percent }
            : { version: desktopUpdate.version },
      };
    }
    case "installing":
      return {
        key: "update.desktop.installing",
        values: { version: desktopUpdate.version },
      };
    case "failed":
      return { key: "update.desktop.failed" };
    case "idle":
      return { key: "settings.desktopVersionDetail" };
  }
}

export function getDesktopUpdateAction(
  desktopUpdate: DesktopUpdateState,
): DesktopUpdateAction {
  switch (desktopUpdate.kind) {
    case "idle":
      return {
        appearance: "default",
        disabled: false,
        spinning: false,
        operation: "check",
        label: { key: "action.checkUpdate" },
      };
    case "checking":
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label: { key: "action.checkingUpdate" },
      };
    case "available":
      return {
        appearance: "primary",
        disabled: false,
        spinning: false,
        operation: "install",
        label: {
          key: "action.updateDesktopVersion",
          values: { version: desktopUpdate.version },
        },
      };
    case "preparing":
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label: { key: "action.updatingDesktop" },
      };
    case "downloading": {
      const percent = downloadPercent(desktopUpdate);
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label:
          percent === null
            ? { key: "action.updatingDesktop" }
            : {
                key: "action.updatingDesktopProgress",
                values: { percent },
              },
      };
    }
    case "installing":
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label: { key: "action.updatingDesktop" },
      };
    case "failed":
      return desktopUpdate.version === null
        ? {
            appearance: "default",
            disabled: false,
            spinning: false,
            operation: "check",
            label: { key: "action.checkUpdate" },
          }
        : {
            appearance: "primary",
            disabled: false,
            spinning: false,
            operation: "install",
            label: {
              key: "action.updateDesktopVersion",
              values: { version: desktopUpdate.version },
            },
          };
  }
}

// ---------------------------------------------------------------------------
// Proxy settings
// ---------------------------------------------------------------------------

export type ProxyDraft = {
  mode: ProxyMode;
  url: string;
  bypass: string;
};

export function proxyDraftFromSettings(settings: ProxySettings): ProxyDraft {
  return { mode: settings.mode, url: settings.url, bypass: settings.bypass };
}

export function proxyDraftAfterSave(
  current: ProxyDraft,
  saved: ProxySettings,
  saveRevision: number,
  currentRevision: number,
): ProxyDraft {
  return saveRevision === currentRevision
    ? proxyDraftFromSettings(saved)
    : current;
}

export function proxySettingsFromDraft(draft: ProxyDraft): ProxySettings {
  if (draft.mode !== "manual") {
    return { mode: draft.mode, url: "", bypass: "" };
  }
  return {
    mode: draft.mode,
    url: draft.url.trim(),
    bypass: draft.bypass.trim(),
  };
}

export function proxyDraftChanged(
  draft: ProxyDraft,
  settings: ProxySettings,
): boolean {
  const current = proxyDraftFromSettings(settings);
  const next = proxySettingsFromDraft(draft);
  return (
    current.mode !== next.mode ||
    current.url !== next.url ||
    current.bypass !== next.bypass
  );
}

const PROXY_URL_SCHEMES = ["http", "https", "socks5", "socks5h"];

/**
 * Client-side mirror of the backend manual-URL rules so obvious mistakes get
 * immediate feedback. The backend remains the authoritative validator; this
 * returns an i18n reason token or null when the draft is acceptable.
 */
export function validateProxyDraft(draft: ProxyDraft): string | null {
  if (draft.mode !== "manual") return null;
  const raw = draft.url.trim();
  if (!raw) return "missing";
  let parsed: URL;
  try {
    parsed = new URL(raw);
  } catch {
    return "invalid";
  }
  const scheme = parsed.protocol.replace(/:$/, "");
  if (!PROXY_URL_SCHEMES.includes(scheme)) return "scheme";
  if (!parsed.hostname) return "host";
  if (parsed.username || parsed.password) return "credentials";
  // A proxy endpoint is scheme://host[:port] only; paths, query strings, and
  // fragments are malformed configuration, matching the backend validator.
  // The delimiters are checked on the raw input because the URL API collapses
  // a trailing `?`/`#` to an empty search/hash, while the Rust backend rejects
  // even those empty components. Credentials were rejected above, so any
  // remaining `?`/`#` in the raw input is a query/fragment delimiter, never a
  // percent-encoded character (`%3F`/`%23` contain no literal separator).
  if (raw.includes("?") || raw.includes("#")) return "path";
  if (parsed.pathname !== "" && parsed.pathname !== "/") return "path";
  return null;
}

/** Localized label for a backend proxy validation failure. */
export function proxyValidationErrorKey(reason: string | null): string {
  return `settings.proxyError.${reason ?? "invalid"}`;
}

/** Summary copy for a successful connection test (first working source). */
export function proxyTestSuccessCopy(report: ProxyTestReport): CopySpec | null {
  const source = report.sources[0];
  if (!source) return null;
  return {
    key: "settings.proxyTestSuccess",
    values: { source: source.source, version: source.version },
  };
}

/** Localized, actionable label for one classified test failure. */
export function proxyTestFailureCopy(
  failure: ProxyTestReport["failures"][number],
): CopySpec {
  return {
    key: `settings.proxyKind.${failure.kind}`,
    values: { source: failure.source },
  };
}
