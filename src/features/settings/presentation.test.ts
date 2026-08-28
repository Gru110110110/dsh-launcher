import { describe, expect, it } from "vitest";
import type {
  ProxySettings,
  ProxyTestReport,
} from "@/platform/generated/bindings";
import {
  getDesktopUpdateAction,
  getDesktopUpdateDetail,
  proxyDraftAfterSave,
  proxyDraftChanged,
  proxyDraftFromSettings,
  proxySettingsFromDraft,
  proxyTestFailureCopy,
  proxyTestSuccessCopy,
  proxyValidationErrorKey,
  validateProxyDraft,
} from "./presentation";

describe("settings presentation", () => {
  it("shows the available desktop version", () => {
    expect(
      getDesktopUpdateDetail({ kind: "available", version: "0.3.0" }),
    ).toEqual({
      key: "update.desktop.available",
      values: { version: "0.3.0" },
    });
    expect(
      getDesktopUpdateAction({ kind: "available", version: "0.3.0" }),
    ).toEqual({
      appearance: "primary",
      disabled: false,
      spinning: false,
      operation: "install",
      label: {
        key: "action.updateDesktopVersion",
        values: { version: "0.3.0" },
      },
    });
  });

  it("uses a disabled checking-style action as soon as updating starts", () => {
    expect(
      getDesktopUpdateAction({ kind: "preparing", version: "0.3.0" }),
    ).toEqual({
      appearance: "default",
      disabled: true,
      spinning: true,
      operation: null,
      label: { key: "action.updatingDesktop" },
    });
    expect(
      getDesktopUpdateAction({ kind: "installing", version: "0.3.0" }),
    ).toEqual({
      appearance: "default",
      disabled: true,
      spinning: true,
      operation: null,
      label: { key: "action.updatingDesktop" },
    });
  });

  it("shows bounded download progress when the total is known", () => {
    expect(
      getDesktopUpdateDetail({
        kind: "downloading",
        version: "0.3.0",
        done: 75,
        total: 100,
      }),
    ).toEqual({
      key: "update.desktop.downloading",
      values: { version: "0.3.0", percent: 75 },
    });
    expect(
      getDesktopUpdateDetail({
        kind: "downloading",
        version: "0.3.0",
        done: 150,
        total: 100,
      }).values?.percent,
    ).toBe(100);
    expect(
      getDesktopUpdateAction({
        kind: "downloading",
        version: "0.3.0",
        done: 10,
        total: 100,
      }),
    ).toMatchObject({
      appearance: "default",
      disabled: true,
      spinning: true,
      operation: null,
      label: {
        key: "action.updatingDesktopProgress",
        values: { percent: 10 },
      },
    });
  });

  it("does not invent progress when the total is unknown", () => {
    expect(
      getDesktopUpdateDetail({
        kind: "downloading",
        version: "0.3.0",
        done: 75,
        total: null,
      }),
    ).toEqual({
      key: "update.desktop.downloadingUnknown",
      values: { version: "0.3.0" },
    });
    expect(
      getDesktopUpdateAction({
        kind: "downloading",
        version: "0.3.0",
        done: 75,
        total: null,
      }).label,
    ).toEqual({ key: "action.updatingDesktop" });
  });

  it("keeps check and retry actions reachable after terminal states", () => {
    expect(getDesktopUpdateAction({ kind: "idle" })).toMatchObject({
      appearance: "default",
      disabled: false,
      operation: "check",
    });
    expect(
      getDesktopUpdateAction({ kind: "failed", version: null }),
    ).toMatchObject({
      appearance: "default",
      disabled: false,
      operation: "check",
    });
    expect(
      getDesktopUpdateAction({ kind: "failed", version: "0.3.0" }),
    ).toMatchObject({
      appearance: "primary",
      disabled: false,
      operation: "install",
      label: {
        key: "action.updateDesktopVersion",
        values: { version: "0.3.0" },
      },
    });
  });
});

describe("proxy presentation", () => {
  const saved = { mode: "manual", url: "http://127.0.0.1:7890", bypass: "" };

  it("round-trips drafts and detects changes", () => {
    expect(proxyDraftFromSettings(saved as ProxySettings)).toEqual({
      mode: "manual",
      url: "http://127.0.0.1:7890",
      bypass: "",
    });
    expect(
      proxyDraftChanged(
        { mode: "manual", url: "http://127.0.0.1:7890", bypass: "" },
        saved as ProxySettings,
      ),
    ).toBe(false);
    expect(
      proxyDraftChanged(
        { mode: "manual", url: " http://127.0.0.1:7890 ", bypass: " " },
        saved as ProxySettings,
      ),
    ).toBe(false);
    expect(
      proxyDraftChanged(
        { mode: "direct", url: "http://127.0.0.1:7890", bypass: "" },
        saved as ProxySettings,
      ),
    ).toBe(true);
    expect(
      proxySettingsFromDraft({
        mode: "direct",
        url: "http://user:topsecret@proxy.invalid:8080",
        bypass: "localhost",
      }),
    ).toEqual({ mode: "direct", url: "", bypass: "" });
  });

  it("does not overwrite edits made while a proxy save is in flight", () => {
    const edited = {
      mode: "manual" as const,
      url: "http://127.0.0.1:9090",
      bypass: "localhost",
    };
    const savedSettings = {
      mode: "manual" as const,
      url: "http://127.0.0.1:8080",
      bypass: "",
    };

    expect(proxyDraftAfterSave(edited, savedSettings, 4, 5)).toBe(edited);
    expect(proxyDraftAfterSave(edited, savedSettings, 5, 5)).toEqual({
      mode: "manual",
      url: "http://127.0.0.1:8080",
      bypass: "",
    });
  });

  it("accepts every supported manual scheme", () => {
    for (const scheme of ["http", "https", "socks5", "socks5h"]) {
      expect(
        validateProxyDraft({
          mode: "manual",
          url: `${scheme}://127.0.0.1:1080`,
          bypass: "",
        }),
      ).toBeNull();
    }
    // A bare root path is fine.
    expect(
      validateProxyDraft({
        mode: "manual",
        url: "http://127.0.0.1:8080/",
        bypass: "",
      }),
    ).toBeNull();
  });

  it("rejects invalid manual URLs with stable reason tokens", () => {
    const cases: [string, string][] = [
      ["", "missing"],
      ["   ", "missing"],
      ["not-a-url", "invalid"],
      ["ftp://127.0.0.1:21", "scheme"],
      ["http://user:pw@127.0.0.1:8080", "credentials"],
      ["http://user@127.0.0.1:8080", "credentials"],
      ["http://127.0.0.1:8080/proxy", "path"],
      ["http://127.0.0.1:8080?x=1", "path"],
      ["http://127.0.0.1:8080#frag", "path"],
      // Empty query/fragment delimiters are rejected like the Rust backend,
      // even though the URL API reports empty search/hash for them.
      ["http://proxy.example?", "path"],
      ["http://proxy.example#", "path"],
      ["socks5://127.0.0.1:1080/extra", "path"],
    ];
    for (const [url, reason] of cases) {
      expect(validateProxyDraft({ mode: "manual", url, bypass: "" })).toBe(
        reason,
      );
      expect(proxyValidationErrorKey(reason)).toBe(
        `settings.proxyError.${reason}`,
      );
    }
  });

  it("skips URL validation outside manual mode", () => {
    expect(
      validateProxyDraft({ mode: "system", url: "not-a-url", bypass: "" }),
    ).toBeNull();
    expect(
      validateProxyDraft({ mode: "direct", url: "", bypass: "" }),
    ).toBeNull();
  });

  it("summarizes successful and failed connection tests", () => {
    const report: ProxyTestReport = {
      sources: [{ source: "registry.npmmirror.com", version: "1.2.3" }],
      failures: [
        {
          source: "registry.npmjs.org",
          kind: "proxyAuth",
          detail: "proxy requires authentication",
        },
      ],
    };
    expect(proxyTestSuccessCopy(report)).toEqual({
      key: "settings.proxyTestSuccess",
      values: { source: "registry.npmmirror.com", version: "1.2.3" },
    });
    const failure = report.failures[0];
    expect(failure).toBeDefined();
    if (!failure) return;
    expect(proxyTestFailureCopy(failure)).toEqual({
      key: "settings.proxyKind.proxyAuth",
      values: { source: "registry.npmjs.org" },
    });
    expect(proxyTestSuccessCopy({ sources: [], failures: [] })).toBeNull();
  });
});
