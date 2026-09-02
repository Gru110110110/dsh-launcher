import type {
  ProxyMode,
  ProxySettings,
  ProxyTestReport,
} from "@/platform/generated/bindings";

type CopySpec = {
  key: string;
  values?: Record<string, string | number>;
};

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
