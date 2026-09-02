// DSH Launcher desktop-pet bridge. It runs inside the Harness host, reduces
// session events to a bounded five-state snapshot, and exposes only sanitized
// state over a token-protected loopback endpoint.

import crypto from "node:crypto";
import http from "node:http";

export const name = "dsh-desktop-pet-bridge";

const MAX_SESSIONS = 256;
const MAX_TEXT = 160;
const EVENT_HISTORY = 128;

export const PetState = Object.freeze({
  WAITING: "waiting",
  ERROR: "error",
  WORKING: "working",
  THINKING: "thinking",
  IDLE: "idle",
});

const statePriority = Object.freeze({
  [PetState.WAITING]: 60,
  [PetState.ERROR]: 50,
  [PetState.WORKING]: 30,
  [PetState.THINKING]: 20,
  [PetState.IDLE]: 0,
});

function boundedText(value, max = MAX_TEXT) {
  if (value === null || value === undefined) return undefined;
  // eslint-disable-next-line no-control-regex
  const text = String(value).replace(/[\x00-\x1f\x7f]/gu, " ").replace(/\s+/gu, " ").trim();
  return text ? text.slice(0, max) : undefined;
}

function cleanProjectName(value) {
  const text = boundedText(value, 240);
  if (!text) return undefined;
  const parts = text.split(/[\\/]/u).filter(Boolean);
  return boundedText(parts.length > 1 ? parts.at(-1) : text, 40);
}

function projectNameOf(session, event) {
  return [
    event?.data?.projectName,
    session?.cwd,
    session?.context?.cwd,
    session?.header?.cwd,
    event?.data?.cwd,
    session?.title,
    session?.name,
    session?.header?.title,
    session?.header?.name,
  ].map(cleanProjectName).find(Boolean);
}

function sessionIdOf(session) {
  return String(session?.header?.id ?? session?.id ?? "unknown-session");
}

function isSubagent(session) {
  return session?.header?.origin === "subagent"
    || Number(session?.header?.delegationDepth ?? 0) > 0;
}

function toolCallIdOf(event, fallback = "") {
  const content = event?.data?.message?.content;
  const contentCallId = Array.isArray(content)
    ? content.find((item) => item?.toolCallId)?.toolCallId
    : undefined;
  return String(event?.data?.message?.source?.callId
    ?? contentCallId
    ?? event?.data?.message?.toolCallId
    ?? event?.data?.message?.callId
    ?? event?.data?.callId
    ?? fallback);
}

function toolActivity(name) {
  const value = String(name || "").toLowerCase();
  if (/search|grep|find|glob|web|read|fetch|open/u.test(value)) return "searching";
  if (/write|edit|patch|replace|create|move|delete/u.test(value)) return "editing";
  if (/test|check|lint|build|verify/u.test(value)) return "testing";
  if (/shell|bash|exec|command|terminal|powershell/u.test(value)) return "commanding";
  return "using-tool";
}

function isUserQuestionTool(name) {
  const tokens = String(name || "").toLowerCase().split(/[^a-z0-9]+/u).filter(Boolean);
  const asks = new Set(["ask", "request", "requests", "require", "requires", "prompt", "need", "needs", "seek", "seeks"]);
  const users = new Set(["user", "human", "me"]);
  const nouns = new Set(["question", "questions", "input", "answer", "answers", "decision", "decisions", "confirmation", "approval", "permission", "authorization", "authorisation", "consent", "clarify", "clarification"]);
  const hasUserNoun = tokens.some((token, index) => users.has(token) && nouns.has(tokens[index + 1] ?? ""));
  const hasAskNoun = tokens.some((token, index) => asks.has(token)
    && tokens.slice(index + 1, index + 5).some((next) => users.has(next) || nouns.has(next)));
  const strong = tokens.some((token) => token === "authorize" || token === "authorise" || token === "consent");
  const exitsPlan = tokens.some((token, index) => token === "exit" && tokens[index + 1] === "plan" && tokens[index + 2] === "mode");
  return hasUserNoun || hasAskNoun || strong || exitsPlan;
}

function progressOf(todos) {
  if (!Array.isArray(todos) || todos.length === 0) return undefined;
  const completed = todos.filter((todo) => ["completed", "complete", "done"].includes(todo?.status)).length;
  return { completed, total: todos.length };
}

function compareRecords(left, right) {
  return (statePriority[right.state] ?? 0) - (statePriority[left.state] ?? 0)
    || right.updatedAt - left.updatedAt
    || left.id.localeCompare(right.id);
}

export class PetReducer {
  constructor({ maxSessions = MAX_SESSIONS } = {}) {
    this.sessions = new Map();
    this.maxSessions = maxSessions;
    this.clock = 0;
    this.signature = undefined;
  }

  handle(session, event) {
    if (!event || typeof event.type !== "string" || isSubagent(session)) return undefined;
    const record = this.#record(sessionIdOf(session));
    record.project = projectNameOf(session, event) ?? record.project;

    switch (event.type) {
      case "turn/start":
        record.turnActive = true;
        record.openTools.clear();
        record.waitingCallId = undefined;
        record.waitingApprovalId = undefined;
        record.task = undefined;
        record.progress = undefined;
        this.#update(record, PetState.THINKING, "turn-start");
        break;
      case "step/start":
      case "assistant/chunk":
      case "assistant/message":
        if (!record.turnActive || record.openTools.size > 0) return undefined;
        this.#update(record, PetState.THINKING, "thinking");
        break;
      case "tool/call": {
        const callId = toolCallIdOf(event, `seq-${String(event.seq ?? "unknown")}`);
        const toolName = boundedText(event.data?.name ?? event.data?.message?.name ?? "tool", 80) ?? "tool";
        record.openTools.set(callId, toolName);
        if (isUserQuestionTool(toolName)) {
          record.waitingCallId = callId;
          this.#update(record, PetState.WAITING, "user-question", toolName);
        } else {
          this.#update(record, PetState.WORKING, "tool-call", toolActivity(toolName), toolName);
        }
        break;
      }
      case "tool/result": {
        const callId = toolCallIdOf(event);
        if (callId) record.openTools.delete(callId);
        if (callId === record.waitingCallId) record.waitingCallId = undefined;
        if (event.data?.error) {
          this.#update(record, PetState.ERROR, "tool-error");
        } else {
          this.#resume(record, "tool-result");
        }
        break;
      }
      case "user/message":
        if (!record.waitingCallId) return undefined;
        record.openTools.delete(record.waitingCallId);
        record.waitingCallId = undefined;
        this.#resume(record, "user-message");
        break;
      case "approval/asked":
        record.waitingApprovalId = String(event.data?.id ?? "");
        this.#update(record, PetState.WAITING, "approval", undefined, boundedText(event.data?.toolName, 80));
        break;
      case "approval/decided":
        if (!record.waitingApprovalId || String(event.data?.id ?? "") !== record.waitingApprovalId) return undefined;
        record.waitingApprovalId = undefined;
        this.#resume(record, "approval-decided");
        break;
      case "todo/write": {
        const todos = Array.isArray(event.data?.todos) ? event.data.todos : [];
        const current = todos.find((todo) => todo?.status === "in_progress")
          ?? todos.find((todo) => todo?.status === "pending");
        record.task = boundedText(current?.content, 120) ?? record.task;
        record.progress = progressOf(todos) ?? record.progress;
        record.updatedAt = ++this.clock;
        break;
      }
      case "turn/end": {
        record.turnActive = false;
        record.openTools.clear();
        record.waitingCallId = undefined;
        record.waitingApprovalId = undefined;
        const kind = String(event.data?.reason?.kind ?? "completed");
        if (kind === "blocked") this.#update(record, PetState.WAITING, "turn-blocked");
        else if (kind === "completed" || kind === "aborted") this.#update(record, PetState.IDLE, `turn-${kind}`);
        else this.#update(record, PetState.ERROR, `turn-${boundedText(kind, 40) ?? "error"}`);
        break;
      }
      default:
        return undefined;
    }
    return this.#render();
  }

  disposeSession(session) {
    if (!this.sessions.delete(sessionIdOf(session))) return undefined;
    return this.#render();
  }

  #record(id) {
    let record = this.sessions.get(id);
    if (record) return record;
    record = {
      id,
      state: PetState.IDLE,
      phase: "session-created",
      turnActive: false,
      openTools: new Map(),
      updatedAt: ++this.clock,
    };
    this.sessions.set(id, record);
    if (this.sessions.size > this.maxSessions) {
      const candidates = [...this.sessions.values()].filter((item) => item !== record);
      candidates.sort((left, right) => left.updatedAt - right.updatedAt);
      if (candidates[0]) this.sessions.delete(candidates[0].id);
    }
    return record;
  }

  #resume(record, phase) {
    if (record.openTools.size > 0) {
      const toolName = record.openTools.values().next().value;
      this.#update(record, PetState.WORKING, phase, toolActivity(toolName), toolName);
    } else {
      this.#update(record, PetState.THINKING, phase);
    }
  }

  #update(record, state, phase, activity, toolName) {
    record.state = state;
    record.phase = phase;
    record.activity = activity;
    record.toolName = toolName;
    record.updatedAt = ++this.clock;
  }

  #render() {
    const records = [...this.sessions.values()].sort(compareRecords);
    const record = records[0] ?? {
      id: "dsh-host",
      state: PetState.IDLE,
      phase: "no-session",
    };
    const signature = [
      record.id,
      record.state,
      record.phase,
      record.activity ?? "",
      record.toolName ?? "",
      record.project ?? "",
      record.task ?? "",
      record.progress?.completed ?? "",
      record.progress?.total ?? "",
    ].join("|");
    if (signature === this.signature) return undefined;
    this.signature = signature;
    return {
      state: record.state,
      phase: record.phase,
      activity: record.activity,
      toolName: record.toolName,
      project: record.project,
      task: record.task,
      progress: record.progress,
    };
  }
}

function parseListenAddr(value) {
  const match = /^(127\.0\.0\.1|\[::1\]):([0-9]{1,5})$/u.exec(value);
  if (!match) return undefined;
  const port = Number(match[2]);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return undefined;
  return { host: match[1] === "[::1]" ? "::1" : match[1], port };
}

function authorized(req, token) {
  const candidate = req.headers["x-dsh-pet-token"];
  if (typeof candidate !== "string") return false;
  const left = Buffer.from(candidate);
  const right = Buffer.from(token);
  return left.length === right.length && crypto.timingSafeEqual(left, right);
}

function json(res, status, payload) {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
    "content-length": Buffer.byteLength(body),
  });
  res.end(body);
}

export function createPetServer({ token, initialState }) {
  let sequence = 0;
  let snapshot = {
    version: 1,
    sequence,
    state: PetState.IDLE,
    phase: "bridge-start",
    updatedAtMs: Date.now(),
    ...initialState,
  };
  const history = [];
  const clients = new Set();

  const writeEvent = (res, value) => {
    res.write(`id: ${String(value.sequence)}\nevent: state\ndata: ${JSON.stringify(value)}\n\n`);
  };
  const publish = (next) => {
    snapshot = {
      version: 1,
      sequence: ++sequence,
      state: next.state,
      phase: next.phase,
      activity: boundedText(next.activity, 40),
      toolName: boundedText(next.toolName, 80),
      project: boundedText(next.project, 40),
      task: boundedText(next.task, 120),
      progress: next.progress,
      updatedAtMs: Date.now(),
    };
    history.push(snapshot);
    if (history.length > EVENT_HISTORY) history.shift();
    for (const client of clients) writeEvent(client, snapshot);
    return snapshot;
  };

  const server = http.createServer((req, res) => {
    if (!authorized(req, token)) {
      json(res, 404, { error: "not found" });
      return;
    }
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    if (req.method === "GET" && url.pathname === "/pet/state") {
      json(res, 200, snapshot);
      return;
    }
    if (req.method !== "GET" || url.pathname !== "/pet/events") {
      json(res, 404, { error: "not found" });
      return;
    }
    res.writeHead(200, {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-store",
      connection: "keep-alive",
      "x-accel-buffering": "no",
    });
    res.write("retry: 500\n\n");
    const since = Number(url.searchParams.get("since") ?? "-1");
    const replay = history.filter((item) => item.sequence > since);
    if (replay.length > 0) replay.forEach((item) => writeEvent(res, item));
    else if (snapshot.sequence > since) writeEvent(res, snapshot);
    clients.add(res);
    const remove = () => clients.delete(res);
    req.once("close", remove);
    res.once("close", remove);
  });

  const heartbeat = setInterval(() => {
    for (const client of clients) client.write(": heartbeat\n\n");
  }, 2_000);
  heartbeat.unref?.();
  server.once("close", () => clearInterval(heartbeat));
  return { server, publish, snapshot: () => snapshot };
}

export async function apply(ctx) {
  const log = (level, message) => {
    try { ctx?.logger?.[level]?.(message); } catch { /* optional logger */ }
  };
  try {
    const address = parseListenAddr(process.env.DSH_DESKTOP_PET_LISTEN ?? "");
    const token = process.env.DSH_DESKTOP_PET_TOKEN;
    if (!address || !token) {
      log("warn", "pet-bridge: missing or invalid environment; feature unavailable");
      return;
    }
    const reducer = new PetReducer();
    const bridge = createPetServer({ token });
    const listening = await new Promise((resolve) => {
      const onError = () => resolve(false);
      bridge.server.once("error", onError);
      bridge.server.listen(address.port, address.host, () => {
        bridge.server.removeListener("error", onError);
        resolve(true);
      });
    }).catch(() => false);
    if (!listening || !bridge.server.listening) {
      try { bridge.server.close(); } catch { /* best effort */ }
      return;
    }
    const offEvent = ctx.on?.("session/event", (session, event) => {
      try {
        const next = reducer.handle(session, event);
        if (next) bridge.publish(next);
      } catch (error) {
        log("error", `pet-bridge: session event failed: ${boundedText(error?.message, 120) ?? "unknown"}`);
      }
    }, { global: true });
    const offDisposed = ctx.on?.("session/disposed", (session) => {
      try {
        const next = reducer.disposeSession(session);
        if (next) bridge.publish(next);
      } catch (error) {
        log("error", `pet-bridge: session disposal failed: ${boundedText(error?.message, 120) ?? "unknown"}`);
      }
    }, { global: true });
    ctx.effect?.(() => () => {
      offEvent?.();
      offDisposed?.();
      try { bridge.server.close(); } catch { /* best effort */ }
    });
  } catch (error) {
    log("error", `pet-bridge: activation failed: ${boundedText(error?.message, 120) ?? "unknown"}`);
  }
}

export const __testing = {
  boundedText,
  isUserQuestionTool,
  parseListenAddr,
  statePriority,
  toolActivity,
};
