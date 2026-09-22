// All transports are fake. Loopback tests use ephemeral ports and never read
// real credentials or production data.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import net from "node:net";
import { apply, name, __testing } from "./balance-bridge.mjs";

const {
  assertBalanceUrl,
  checkBalanceStatus,
  configuredDeepSeekBaseUrl,
  createBalanceManager,
  createBalanceServer,
  fetchBalanceWithTransport,
  isOfficialDeepSeekEndpoint,
  parseListenAddr,
  resolveApiKey,
  sanitizeDetail,
  validateBalancePayload,
} = __testing;

const okBody = (amount = "12.34") => JSON.stringify({
  is_available: true,
  balance_infos: [{ currency: "CNY", total_balance: amount }],
});

describe("module surface", () => {
  it("is a balance-only Harness plugin", async () => {
    expect(name).toBe("dsh-desktop-balance-bridge");
    const mod = await import("./balance-bridge.mjs");
    expect(mod.default).toBeUndefined();
    expect(__testing.createStreamRecorder).toBeUndefined();
    expect(__testing.PRICING).toBeUndefined();
  });
});

describe("official balance transport contract", () => {
  it("prefers CNY and preserves another currency without conversion", () => {
    expect(validateBalancePayload({
      is_available: true,
      balance_infos: [
        { currency: "USD", total_balance: "9.99" },
        { currency: "CNY", total_balance: "12.34" },
      ],
    })).toEqual({ isAvailable: true, currency: "CNY", totalBalance: "12.34" });
    expect(validateBalancePayload({
      is_available: false,
      balance_infos: [{ currency: "USD", total_balance: "9.99" }],
    })).toEqual({ isAvailable: false, currency: "USD", totalBalance: "9.99" });
  });

  it("strictly rejects malformed payloads", () => {
    expect(() => validateBalancePayload({
      is_available: true,
      balance_infos: [],
    })).toThrow();
    expect(() => validateBalancePayload({ is_available: true, balance_infos: "bad" })).toThrow();
    expect(() => validateBalancePayload({
      is_available: true,
      balance_infos: [{ currency: "CNY", total_balance: 1 }],
    })).toThrow();
  });

  it("maps fake transport failures to stable codes", async () => {
    const invalid = await fetchBalanceWithTransport("fake", async () => "not json");
    expect(invalid).toEqual({ status: "fail", detail: "balanceInvalidResponse" });
    const timeout = await fetchBalanceWithTransport("fake", async () => {
      const error = new Error("timeout");
      error.code = "balanceTimeout";
      throw error;
    });
    expect(timeout).toEqual({ status: "fail", detail: "balanceTimeout" });
  });

  it("accepts only the exact official HTTPS origin and rejects redirects", () => {
    expect(() => assertBalanceUrl("https://api.deepseek.com/user/balance")).not.toThrow();
    for (const bad of [
      "http://api.deepseek.com/user/balance",
      "https://api.deepseek.com.evil.test/user/balance",
      "https://api.deepseek.com@evil.test/user/balance",
    ]) expect(() => assertBalanceUrl(bad)).toThrow();
    expect(() => checkBalanceStatus(200)).not.toThrow();
    expect(() => checkBalanceStatus(302)).toThrow();
  });
});

describe("balance cache", () => {
  it("expires before the desktop's five-minute poll and refresh bypasses it", async () => {
    let now = 1_000;
    const transport = vi.fn(async () => okBody("7.50"));
    const manager = createBalanceManager({
      resolveKey: async () => "fake-key",
      transport,
      now: () => now,
    });
    expect((await manager.fetch(false)).totalBalance).toBe("7.50");
    now += 100;
    await manager.fetch(false);
    expect(transport).toHaveBeenCalledTimes(1);
    now += 270_000;
    await manager.fetch(false);
    expect(transport).toHaveBeenCalledTimes(2);
    await manager.fetch(true);
    expect(transport).toHaveBeenCalledTimes(3);
  });

  it("keeps the last successful value on query failure and never fakes zero", async () => {
    const transport = vi
      .fn()
      .mockResolvedValueOnce(okBody("8.88"))
      .mockRejectedValueOnce(new Error("offline"));
    const manager = createBalanceManager({
      resolveKey: async () => "fake-key",
      transport,
      cacheMs: 0,
    });
    await manager.fetch(false);
    const stale = await manager.fetch(true);
    expect(stale).toMatchObject({
      status: "stale",
      detail: "balanceHttpError",
      totalBalance: "8.88",
    });

    const empty = createBalanceManager({
      resolveKey: async () => "fake-key",
      transport: async () => { throw new Error("offline"); },
    });
    expect(await empty.fetch(false)).toMatchObject({
      status: "unavailable",
      totalBalance: null,
    });
  });

  it("does not resolve or send a custom gateway credential", async () => {
    const resolveKey = vi.fn(async () => {
      const error = new Error("custom");
      error.code = "balanceNonOfficialEndpoint";
      throw error;
    });
    const transport = vi.fn();
    const manager = createBalanceManager({ resolveKey, transport });
    expect(await manager.fetch(false)).toMatchObject({
      status: "unavailable",
      detail: "balanceNonOfficialEndpoint",
    });
    expect(transport).not.toHaveBeenCalled();
  });

  it("reports credential resolver failures as unavailable, not missing", async () => {
    const error = Object.assign(new Error("credentials offline"), {
      code: "balanceUnavailable",
    });
    const manager = createBalanceManager({
      resolveKey: async () => { throw error; },
      transport: vi.fn(),
    });
    expect(await manager.fetch(false)).toMatchObject({
      status: "unavailable",
      detail: "balanceUnavailable",
    });
  });
});

describe("endpoint configuration", () => {
  // Harness <= 0.1.6 resolved a settings namespace through `settings.get(ns)`.
  const legacySettings = (baseURL, apiKeyEnv) => ({
    get: () => ({ baseURL, ...(apiKeyEnv === undefined ? {} : { apiKeyEnv }) }),
  });
  // Harness 0.1.7 replaced that service with profile-patch forms keyed by entry id.
  const formsSettings = (value) => ({
    describe: () => [
      { ns: "some-other-plugin", value: { baseURL: "https://relay.example" } },
      { ns: "llm-deepseek", value },
    ],
  });
  const contextFor = (settings) => ({
    get(key) {
      if (key === "settings") return settings;
      return undefined;
    },
  });

  it("recognizes only the official DeepSeek endpoint through the legacy settings API", () => {
    const context = (baseURL) => contextFor(legacySettings(baseURL));
    expect(configuredDeepSeekBaseUrl(context("https://api.deepseek.com"))).toBe("https://api.deepseek.com");
    expect(isOfficialDeepSeekEndpoint(context("https://api.deepseek.com"))).toBe(true);
    expect(isOfficialDeepSeekEndpoint(context("https://gateway.example"))).toBe(false);
  });

  it("reads the profile-patch form API shipped with Harness 0.1.7", () => {
    const official = contextFor(formsSettings({
      apiKeyEnv: "DEEPSEEK_API_KEY",
      baseURL: "https://api.deepseek.com",
    }));
    expect(configuredDeepSeekBaseUrl(official)).toBe("https://api.deepseek.com");
    expect(isOfficialDeepSeekEndpoint(official)).toBe(true);

    const custom = contextFor(formsSettings({ baseURL: "https://relay.example/v1" }));
    expect(configuredDeepSeekBaseUrl(custom)).toBe("https://relay.example/v1");
    expect(isOfficialDeepSeekEndpoint(custom)).toBe(false);
  });

  it("falls back to the public default whenever no endpoint is configured", () => {
    for (const context of [
      contextFor(legacySettings(undefined)),
      contextFor(formsSettings({})),
      { get: () => undefined },
    ]) {
      expect(configuredDeepSeekBaseUrl(context)).toBe("https://api.deepseek.com");
      expect(isOfficialDeepSeekEndpoint(context)).toBe(true);
    }
  });

  it("reports an unreadable settings API as unavailable, never as a custom endpoint", async () => {
    // Harness 0.1.7 renamed the settings read API; a future rename or a broken
    // provider must not surface as "you configured a custom API address".
    const unknownApi = contextFor({});
    expect(configuredDeepSeekBaseUrl(unknownApi)).toBeNull();
    await expect(resolveApiKey(unknownApi)).rejects.toMatchObject({ code: "balanceUnavailable" });

    const throwing = contextFor({ describe: () => { throw new Error("settings offline"); } });
    expect(configuredDeepSeekBaseUrl(throwing)).toBeNull();
    await expect(resolveApiKey(throwing)).rejects.toMatchObject({ code: "balanceUnavailable" });
  });

  it("refuses a custom endpoint before resolving any credential", async () => {
    const resolve = vi.fn();
    const context = {
      get(key) {
        if (key === "settings") return formsSettings({ baseURL: "https://relay.example" });
        if (key === "credentials") return { resolve };
        return undefined;
      },
    };
    await expect(resolveApiKey(context)).rejects.toMatchObject({ code: "balanceNonOfficialEndpoint" });
    expect(resolve).not.toHaveBeenCalled();
  });

  it("resolves the credential reference the settings select on both APIs", async () => {
    const resolve = vi.fn(async () => ({ value: "fake-key" }));
    const context = (settings) => ({
      get(key) {
        if (key === "settings") return settings;
        if (key === "credentials") return { resolve };
        return undefined;
      },
    });
    await expect(resolveApiKey(context(legacySettings("https://api.deepseek.com", "LEGACY_KEY"))))
      .resolves.toBe("fake-key");
    await expect(resolveApiKey(context(formsSettings({
      baseURL: "https://api.deepseek.com",
      apiKeyEnv: "FORMS_KEY",
    })))).resolves.toBe("fake-key");
    expect(resolve.mock.calls.map(([ref]) => ref)).toEqual(["LEGACY_KEY", "FORMS_KEY"]);
  });

  it("parses only exact IPv4 loopback addresses", () => {
    expect(parseListenAddr("127.0.0.1:1234")).toEqual({ host: "127.0.0.1", port: 1234 });
    for (const bad of ["0.0.0.0:1", "localhost:1", "[::1]:1", "127.0.0.1:0", "bad"]) {
      expect(parseListenAddr(bad)).toBeNull();
    }
  });
});

describe("token-guarded loopback server", () => {
  let server;
  let base;
  const token = "a".repeat(64);
  const calls = [];

  beforeEach(async () => {
    server = createBalanceServer({
      token,
      balanceFetcher: async (refresh) => {
        calls.push(refresh);
        return { version: 1, status: "ok", totalBalance: "1.00" };
      },
    });
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", resolve);
    });
    base = `http://127.0.0.1:${server.address().port}`;
  });

  afterEach(async () => {
    calls.length = 0;
    if (server?.listening) await new Promise((resolve) => server.close(resolve));
  });

  it("hides wrong tokens and unknown routes behind 404", async () => {
    expect((await fetch(`${base}/balance`)).status).toBe(404);
    expect((await fetch(`${base}/balance`, { headers: { "x-dsh-balance-token": "b".repeat(64) } })).status).toBe(404);
    expect((await fetch(`${base}/snapshot`, { headers: { "x-dsh-balance-token": token } })).status).toBe(404);
    expect((await fetch(`${base}/balance`, { method: "POST", headers: { "x-dsh-balance-token": token } })).status).toBe(404);
  });

  it("serves only balance and passes the manual refresh flag", async () => {
    const headers = { "x-dsh-balance-token": token };
    expect((await fetch(`${base}/balance`, { headers })).status).toBe(200);
    expect((await fetch(`${base}/balance?refresh=1`, { headers })).status).toBe(200);
    expect(calls).toEqual([false, true]);
  });
});

describe("boot safety", () => {
  const envKeys = ["DSH_DESKTOP_BALANCE_LISTEN", "DSH_DESKTOP_BALANCE_TOKEN"];
  const old = {};

  beforeEach(() => {
    for (const key of envKeys) {
      old[key] = process.env[key];
      delete process.env[key];
    }
  });
  afterEach(() => {
    for (const key of envKeys) {
      if (old[key] === undefined) delete process.env[key];
      else process.env[key] = old[key];
    }
  });

  it("activates cleanly without bridge environment", async () => {
    await expect(apply({ logger: {} })).resolves.toBeUndefined();
  });

  it("binds, resolves via credentials, and disposes with the Harness scope", async () => {
    const probe = net.createServer();
    await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
    const port = probe.address().port;
    await new Promise((resolve) => probe.close(resolve));
    process.env.DSH_DESKTOP_BALANCE_LISTEN = `127.0.0.1:${port}`;
    process.env.DSH_DESKTOP_BALANCE_TOKEN = "f".repeat(64);
    let dispose;
    const ctx = {
      get(key) {
        if (key === "credentials") return { resolve: async () => ({ value: "fake-key" }) };
        if (key === "settings") return { get: () => ({ baseURL: "https://api.deepseek.com" }) };
        return undefined;
      },
      effect(factory) { dispose = factory(); },
      logger: {},
    };
    await apply(ctx);
    expect(typeof dispose).toBe("function");
    dispose();
  });
});

describe("sanitization", () => {
  it("strips control characters and caps details", () => {
    expect(sanitizeDetail("bad\n\u0007text")).toBe("badtext");
    expect(sanitizeDetail("x".repeat(500))).toHaveLength(200);
  });
});
