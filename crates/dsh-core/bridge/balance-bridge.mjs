// Minimal DSH Launcher balance bridge running inside the existing Harness
// process, resolves credentials through ctx.credentials, and exposes only a
// sanitized balance result over a token-guarded loopback endpoint.

import crypto from "node:crypto";
import http from "node:http";
import https from "node:https";

export const name = "dsh-desktop-balance-bridge";

const BALANCE_URL = "https://api.deepseek.com/user/balance";
const BALANCE_TIMEOUT_MS = 10_000;
const BALANCE_MAX_BODY = 64 * 1024;
// Keep the bridge cache below the desktop's five-minute polling interval so
// each scheduled poll observes a fresh official balance while quick remounts
// still avoid duplicate upstream requests.
const BALANCE_CACHE_MS = 4.5 * 60 * 1000;

function sanitizeDetail(value) {
  if (value === null || value === undefined) return null;
  // eslint-disable-next-line no-control-regex
  let out = String(value).replace(/[\x00-\x1f\x7f]/g, "").slice(0, 200);
  const token = process.env.DSH_DESKTOP_BALANCE_TOKEN;
  if (token) out = out.split(token).join("[redacted]");
  return out;
}

function assertBalanceUrl(url) {
  const parsed = new URL(url);
  if (parsed.protocol !== "https:" || parsed.host !== "api.deepseek.com") {
    throw new Error("balance URL must be the official HTTPS endpoint");
  }
  return parsed;
}

function configuredDeepSeekBaseUrl(ctx) {
  try {
    const configured = typeof ctx?.get === "function"
      ? ctx.get("settings")?.get("llm-deepseek")?.baseURL
      : undefined;
    if (typeof configured === "string" && configured.length > 0) return configured;
  } catch {
    return null;
  }
  try {
    const env = typeof ctx?.get === "function" ? ctx.get("launchEnvironment") : undefined;
    const configured = env && typeof env.get === "function"
      ? env.get("DEEPSEEK_BASE_URL")?.value
      : process.env.DEEPSEEK_BASE_URL;
    if (typeof configured === "string" && configured.length > 0) return configured;
  } catch {
    return null;
  }
  return "https://api.deepseek.com";
}

function isOfficialDeepSeekEndpoint(ctx) {
  try {
    const url = new URL(configuredDeepSeekBaseUrl(ctx));
    return url.protocol === "https:"
      && url.hostname === "api.deepseek.com"
      && (url.port === "" || url.port === "443")
      && url.username === ""
      && url.password === "";
  } catch {
    return false;
  }
}

function checkBalanceStatus(statusCode) {
  if (statusCode === 200) return;
  const error = new Error("balance http error");
  error.code = "balanceHttpError";
  throw error;
}

async function defaultBalanceTransport(key) {
  const url = assertBalanceUrl(BALANCE_URL);
  return await new Promise((resolve, reject) => {
    let settled = false;
    const fail = (code) => {
      if (settled) return;
      settled = true;
      const error = new Error(code);
      error.code = code;
      reject(error);
    };
    const req = https.request(
      {
        protocol: url.protocol,
        hostname: url.hostname,
        port: 443,
        path: url.pathname,
        method: "GET",
        headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
        timeout: BALANCE_TIMEOUT_MS,
        agent: false,
      },
      (res) => {
        try {
          checkBalanceStatus(res.statusCode ?? 0);
        } catch {
          res.resume();
          fail("balanceHttpError");
          return;
        }
        const chunks = [];
        let size = 0;
        res.on("data", (chunk) => {
          if (settled) return;
          size += chunk.length;
          if (size > BALANCE_MAX_BODY) {
            req.destroy();
            fail("balanceInvalidResponse");
            return;
          }
          chunks.push(chunk);
        });
        res.on("end", () => {
          if (settled) return;
          settled = true;
          resolve(Buffer.concat(chunks).toString("utf8"));
        });
        res.on("error", () => fail("balanceHttpError"));
      },
    );
    req.on("timeout", () => {
      req.destroy();
      fail("balanceTimeout");
    });
    req.on("error", () => fail("balanceHttpError"));
    req.end();
  });
}

function validateBalancePayload(obj) {
  const bad = () => {
    const error = new Error("invalid balance payload");
    error.code = "balanceInvalidResponse";
    return error;
  };
  if (!obj || typeof obj !== "object" || typeof obj.is_available !== "boolean") throw bad();
  if (!Array.isArray(obj.balance_infos)) throw bad();
  const infos = obj.balance_infos.map((entry) => {
    if (!entry || typeof entry !== "object") throw bad();
    if (typeof entry.currency !== "string" || entry.currency.length < 1 || entry.currency.length > 8) throw bad();
    if (typeof entry.total_balance !== "string" || !/^-?\d+(\.\d{1,8})?$/.test(entry.total_balance)) throw bad();
    return { currency: entry.currency, totalBalance: entry.total_balance };
  });
  const chosen = infos.find((entry) => entry.currency === "CNY") ?? infos[0] ?? null;
  if (!chosen) throw bad();
  return {
    isAvailable: obj.is_available,
    currency: chosen?.currency ?? null,
    totalBalance: chosen?.totalBalance ?? null,
  };
}

async function fetchBalanceWithTransport(key, transport) {
  let body;
  try {
    body = await transport(key);
  } catch (error) {
    const detail = error?.code === "balanceTimeout" || error?.code === "balanceInvalidResponse"
      ? error.code
      : "balanceHttpError";
    return { status: "fail", detail };
  }
  try {
    return { status: "ok", ...validateBalancePayload(JSON.parse(body)) };
  } catch {
    return { status: "fail", detail: "balanceInvalidResponse" };
  }
}

function createBalanceManager({ resolveKey, transport, now = () => Date.now(), cacheMs = BALANCE_CACHE_MS }) {
  let lastGood = null;
  const response = (status, detail, value) => ({
    version: 1,
    status,
    detail,
    isAvailable: value?.isAvailable ?? null,
    currency: value?.currency ?? null,
    totalBalance: value?.totalBalance ?? null,
    fetchedAtMs: value?.fetchedAtMs ?? null,
  });
  return {
    async fetch(refresh) {
      if (!refresh && lastGood && now() - lastGood.fetchedAtMs < cacheMs) {
        return response("ok", null, lastGood);
      }
      let key = null;
      let missingDetail = "balanceNoCredential";
      try {
        key = await resolveKey();
      } catch (error) {
        if (error?.code === "balanceNonOfficialEndpoint") {
          missingDetail = "balanceNonOfficialEndpoint";
        } else {
          missingDetail = "balanceUnavailable";
        }
      }
      if (!key) {
        return response(lastGood ? "stale" : "unavailable", missingDetail, lastGood);
      }
      const result = await fetchBalanceWithTransport(key, transport);
      if (result.status === "ok") {
        lastGood = { ...result, fetchedAtMs: now() };
        return response("ok", null, lastGood);
      }
      return response(lastGood ? "stale" : "unavailable", result.detail ?? "balanceUnavailable", lastGood);
    },
  };
}

function sendJson(res, status, value) {
  const body = JSON.stringify(value);
  res.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  res.end(body);
}

function createBalanceServer({ token, balanceFetcher }) {
  const tokenBuffer = Buffer.from(String(token), "utf8");
  return http.createServer((req, res) => {
    (async () => {
      if (req.method !== "GET") return sendJson(res, 404, { error: "not found" });
      const header = req.headers["x-dsh-balance-token"];
      const headerBuffer = typeof header === "string" ? Buffer.from(header, "utf8") : null;
      if (!headerBuffer
        || headerBuffer.length !== tokenBuffer.length
        || !crypto.timingSafeEqual(headerBuffer, tokenBuffer)) {
        return sendJson(res, 404, { error: "not found" });
      }
      const url = new URL(req.url || "/", "http://127.0.0.1");
      if (url.pathname !== "/balance") return sendJson(res, 404, { error: "not found" });
      const result = await balanceFetcher(url.searchParams.get("refresh") === "1");
      return sendJson(res, 200, result);
    })().catch(() => {
      try {
        if (!res.headersSent) sendJson(res, 500, { error: "internal" });
        else res.destroy();
      } catch { /* socket already gone */ }
    });
  });
}

function parseListenAddr(raw) {
  if (typeof raw !== "string") return null;
  const match = /^127\.0\.0\.1:(\d{1,5})$/.exec(raw.trim());
  if (!match) return null;
  const port = Number(match[1]);
  return Number.isInteger(port) && port > 0 && port <= 65535
    ? { host: "127.0.0.1", port }
    : null;
}

const ENV_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

async function resolveApiKey(ctx) {
  if (!isOfficialDeepSeekEndpoint(ctx)) {
    const error = new Error("balance unavailable for a non-official endpoint");
    error.code = "balanceNonOfficialEndpoint";
    throw error;
  }
  let ref = "DEEPSEEK_API_KEY";
  try {
    const configured = typeof ctx.get === "function"
      ? ctx.get("settings")?.get("llm-deepseek")?.apiKeyEnv
      : undefined;
    if (typeof configured === "string" && ENV_NAME_RE.test(configured)) ref = configured;
  } catch { /* default ref */ }
  try {
    const credentials = typeof ctx.get === "function" ? ctx.get("credentials") : undefined;
    if (credentials && typeof credentials.resolve === "function") {
      const hit = await credentials.resolve(ref);
      return typeof hit?.value === "string" && hit.value.length > 0 ? hit.value : null;
    }
  } catch {
    const error = new Error("credential resolution failed");
    error.code = "balanceUnavailable";
    throw error;
  }
  try {
    const env = typeof ctx.get === "function" ? ctx.get("launchEnvironment") : undefined;
    const key = env && typeof env.get === "function" ? env.get(ref)?.value : process.env[ref];
    return typeof key === "string" && key.length > 0 ? key : null;
  } catch {
    const error = new Error("launch environment resolution failed");
    error.code = "balanceUnavailable";
    throw error;
  }
}

export async function apply(ctx) {
  const log = (level, message) => {
    try { ctx?.logger?.[level]?.(message); } catch { /* optional logger */ }
  };
  try {
    const address = parseListenAddr(process.env.DSH_DESKTOP_BALANCE_LISTEN ?? "");
    const token = process.env.DSH_DESKTOP_BALANCE_TOKEN;
    if (!address || !token) {
      log("warn", "balance-bridge: missing or invalid environment; feature unavailable");
      return;
    }
    const manager = createBalanceManager({
      resolveKey: () => resolveApiKey(ctx),
      transport: defaultBalanceTransport,
    });
    const server = createBalanceServer({
      token,
      balanceFetcher: (refresh) => manager.fetch(refresh),
    });
    const listening = await new Promise((resolve) => {
      const onError = () => resolve(false);
      server.once("error", onError);
      server.listen(address.port, address.host, () => {
        server.removeListener("error", onError);
        resolve(true);
      });
    }).catch(() => false);
    if (!listening || !server.listening) {
      try { server.close(); } catch { /* best effort */ }
      return;
    }
    try {
      if (typeof ctx.effect === "function") {
        ctx.effect(() => () => {
          try { server.close(); } catch { /* best effort */ }
        });
      }
    } catch { /* process exit closes it */ }
  } catch (error) {
    log("error", `balance-bridge: activation failed: ${sanitizeDetail(error?.message)}`);
  }
}

export const __testing = {
  assertBalanceUrl,
  checkBalanceStatus,
  configuredDeepSeekBaseUrl,
  createBalanceManager,
  createBalanceServer,
  defaultBalanceTransport,
  fetchBalanceWithTransport,
  isOfficialDeepSeekEndpoint,
  parseListenAddr,
  sanitizeDetail,
  validateBalancePayload,
};
