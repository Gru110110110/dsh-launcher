import { expect, test } from "vitest";

import {
  PetReducer,
  PetState,
  createPetServer,
  __testing,
} from "./pet-bridge.mjs";

const session = { header: { id: "top" }, cwd: "/tmp/demo" };

test("reduces the five public states", () => {
  const reducer = new PetReducer();
  expect(reducer.handle(session, { type: "turn/start", seq: 1 }).state).toBe(
    PetState.WORKING,
  );
  expect(
    reducer.handle(session, {
      type: "assistant/chunk",
      seq: 2,
      data: {
        chunk: { type: "reasoning-delta", index: 0, text: "thinking" },
      },
    }).state,
  ).toBe(PetState.THINKING);
  expect(
    reducer.handle(session, {
      type: "tool/call",
      seq: 3,
      data: { name: "write_file", callId: "a" },
    }).state,
  ).toBe(PetState.WORKING);
  expect(
    reducer.handle(session, {
      type: "approval/asked",
      seq: 4,
      data: { id: "p" },
    }).state,
  ).toBe(PetState.WAITING);
  expect(
    reducer.handle(session, {
      type: "approval/decided",
      seq: 5,
      data: { id: "p" },
    }).state,
  ).toBe(PetState.WORKING);
  expect(
    reducer.handle(session, {
      type: "tool/result",
      seq: 6,
      data: { callId: "a", error: { code: "failed" } },
    }).state,
  ).toBe(PetState.ERROR);
  expect(
    reducer.handle(session, {
      type: "turn/end",
      seq: 7,
      data: { reason: { kind: "completed" } },
    }).state,
  ).toBe(PetState.IDLE);
});

test("shows thinking only for reasoning stream chunks", () => {
  const stateForChunk = (chunk) => {
    const reducer = new PetReducer();
    reducer.handle(session, { type: "turn/start" });
    return reducer.handle(session, {
      type: "assistant/chunk",
      data: { chunk },
    }).state;
  };

  expect(
    stateForChunk({ type: "block-start", index: 0, blockType: "reasoning" }),
  ).toBe(PetState.THINKING);
  expect(
    stateForChunk({ type: "reasoning-delta", index: 0, text: "thinking" }),
  ).toBe(PetState.THINKING);
  expect(
    stateForChunk({ type: "text-delta", index: 0, text: "answering" }),
  ).toBe(PetState.WORKING);
  expect(
    stateForChunk({
      type: "tool-call-delta",
      index: 0,
      id: "search-call",
      name: "find",
      argumentsDelta: "{}",
    }),
  ).toBe(PetState.WORKING);
  expect(
    stateForChunk({
      type: "block-end",
      index: 0,
      block: { type: "reasoning", text: "done" },
    }),
  ).toBe(PetState.WORKING);
});

test("keeps non-reasoning active phases working", () => {
  const reducer = new PetReducer();
  expect(reducer.handle(session, { type: "turn/start" }).state).toBe(
    PetState.WORKING,
  );
  expect(reducer.handle(session, { type: "step/start" }).state).toBe(
    PetState.WORKING,
  );
  expect(
    reducer.handle(session, {
      type: "assistant/message",
      data: { message: { content: [{ type: "text", text: "done" }] } },
    }).state,
  ).toBe(PetState.WORKING);
  expect(
    reducer.handle(session, {
      type: "tool/call",
      data: { name: "find", callId: "search-call" },
    }).state,
  ).toBe(PetState.WORKING);
  expect(
    reducer.handle(session, {
      type: "tool/result",
      data: { callId: "search-call" },
    }).state,
  ).toBe(PetState.WORKING);
});

test("recognizes model-facing tool failures without internal error metadata", () => {
  const reducer = new PetReducer();
  reducer.handle(session, { type: "turn/start" });
  reducer.handle(session, {
    type: "tool/call",
    data: { name: "find", callId: "failed-call" },
  });

  expect(
    reducer.handle(session, {
      type: "tool/result",
      data: {
        message: {
          content: [
            {
              type: "tool-result",
              toolCallId: "failed-call",
              content: [{ type: "text", text: "not found" }],
              isError: true,
            },
          ],
        },
      },
    }).state,
  ).toBe(PetState.ERROR);
});

test("waiting outranks working across sessions", () => {
  const reducer = new PetReducer();
  reducer.handle(session, { type: "turn/start" });
  reducer.handle(session, {
    type: "tool/call",
    data: { name: "shell", callId: "a" },
  });
  const waiting = reducer.handle(
    { header: { id: "other" }, cwd: "/tmp/other" },
    { type: "approval/asked", data: { id: "approval" } },
  );
  expect(waiting.state).toBe(PetState.WAITING);
  expect(waiting.project).toBe("other");
});

test("subagents do not replace the top-level state", () => {
  const reducer = new PetReducer();
  expect(
    reducer.handle(
      { header: { id: "child", origin: "subagent" } },
      { type: "turn/start" },
    ),
  ).toBeUndefined();
});

test("question tool detection avoids ordinary review and permission scans", () => {
  expect(__testing.isUserQuestionTool("request_user_input")).toBe(true);
  expect(__testing.isUserQuestionTool("exit_plan_mode")).toBe(true);
  expect(__testing.isUserQuestionTool("code_review")).toBe(false);
  expect(__testing.isUserQuestionTool("permission_scan")).toBe(false);
});

test("loopback server hides its token and streams versioned snapshots", async () => {
  const bridge = createPetServer({ token: "test-pet-token" });
  await new Promise((resolve, reject) => {
    bridge.server.once("error", reject);
    bridge.server.listen(0, "127.0.0.1", resolve);
  });
  const address = bridge.server.address();
  expect(address).toBeTypeOf("object");
  const base = `http://127.0.0.1:${String(address.port)}`;
  const controller = new AbortController();
  try {
    const unauthorized = await fetch(`${base}/pet/state`);
    expect(unauthorized.status).toBe(404);

    bridge.publish({ state: PetState.WORKING, phase: "test-tool" });
    const current = await fetch(`${base}/pet/state`, {
      headers: { "x-dsh-pet-token": "test-pet-token" },
    });
    expect(await current.json()).toMatchObject({
      version: 1,
      sequence: 1,
      state: PetState.WORKING,
      phase: "test-tool",
    });

    const events = await fetch(`${base}/pet/events?since=0`, {
      headers: { "x-dsh-pet-token": "test-pet-token" },
      signal: controller.signal,
    });
    const reader = events.body.getReader();
    const decoder = new TextDecoder();
    let streamed = "";
    for (
      let index = 0;
      index < 3 && !streamed.includes('"state":"working"');
      index += 1
    ) {
      const chunk = await reader.read();
      if (chunk.done) break;
      streamed += decoder.decode(chunk.value, { stream: true });
    }
    expect(streamed).toContain('"state":"working"');
    await reader.cancel();
  } finally {
    controller.abort();
    await new Promise((resolve) => bridge.server.close(resolve));
  }
});
