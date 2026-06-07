import { describe, expect, it } from "bun:test";
import { gzipSync } from "node:zlib";
import { parseSseStream } from "../../../sse.ts";
import type { CursorProto, ProtoClass, ProtoMessage } from "../proto-loader.ts";
import { decodeCursorStream, encodeConnectFrame } from "../client.ts";
import {
  accumulateCursorResponse,
  cursorUsageToAnthropic,
  translateCursorStream,
} from "./response.ts";
import { createLogger } from "../../../log.ts";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

const fakeProto: CursorProto = {
  AgentServerMessage: jsonProtoClass(),
  AgentClientMessage: jsonProtoClass(),
};

describe("Cursor response translation", () => {
  it("maps usage tokens including cache reads and writes", () => {
    expect(
      cursorUsageToAnthropic({
        inputTokens: "100",
        outputTokens: "7",
        cacheReadTokens: "20",
        cacheWriteTokens: "3",
      }),
    ).toEqual({
      input_tokens: 77,
      output_tokens: 7,
      cache_creation_input_tokens: 3,
      cache_read_input_tokens: 20,
    });
  });

  it("decodes Connect-framed Cursor messages including gzip and end frames", async () => {
    const stream = streamFromChunks([
      frame({ interactionUpdate: { textDelta: { text: "hi" } } }),
      encodeConnectFrame(gzipSync(jsonBytes({ interactionUpdate: { textDelta: { text: "!" } } })), 1),
      encodeConnectFrame(jsonBytes({}), 2),
    ]);

    const events = [];
    for await (const event of decodeCursorStream(stream, fakeProto)) events.push(event);

    expect(events).toEqual([
      { type: "text_delta", text: "hi" },
      { type: "text_delta", text: "!" },
      { type: "end" },
    ]);
  });

  it("accumulates non-streaming thinking, text, usage, and session id", async () => {
    let observedSession: string | undefined;
    const result = await accumulateCursorResponse(
      streamFromChunks([
        frame({ execServerMessage: { requestContextArgs: { notesSessionId: "cursor-session" } } }),
        frame({ interactionUpdate: { thinkingDelta: { text: "think" } } }),
        frame({ interactionUpdate: { textDelta: { text: "hello" } } }),
        frame({ interactionUpdate: { turnEnded: { inputTokens: "10", outputTokens: "2" } } }),
        encodeConnectFrame(jsonBytes({}), 2),
      ]),
      {
        messageId: "msg_1",
        model: "cursor-plan",
        log: createLogger("cursor.response.test"),
        proto: fakeProto,
        onSession: (session) => {
          observedSession = session;
        },
      },
    );

    expect(observedSession).toBe("cursor-session");
    expect(result.response.content).toEqual([
      { type: "thinking", thinking: "think", signature: "" },
      { type: "text", text: "hello" },
    ]);
    expect(result.response.usage.output_tokens).toBe(2);
  });

  it("emits valid Anthropic SSE for thinking and text deltas", async () => {
    const downstream = translateCursorStream(
      streamFromChunks([
        frame({ interactionUpdate: { thinkingDelta: { text: "plan" } } }),
        frame({ interactionUpdate: { textDelta: { text: "done" } } }),
        frame({ interactionUpdate: { turnEnded: { inputTokens: "8", outputTokens: "3" } } }),
        encodeConnectFrame(jsonBytes({}), 2),
      ]),
      {
        messageId: "msg_2",
        model: "cursor-plan",
        log: createLogger("cursor.response.test"),
        proto: fakeProto,
      },
    );

    const events = [];
    for await (const event of parseSseStream(downstream)) {
      events.push({ event: event.event, data: JSON.parse(event.data) });
    }

    expect(events.map((event) => event.event)).toEqual([
      "message_start",
      "ping",
      "content_block_start",
      "content_block_delta",
      "content_block_stop",
      "content_block_start",
      "content_block_delta",
      "content_block_stop",
      "message_delta",
      "message_stop",
    ]);
    expect(events[2]?.data.content_block.type).toBe("thinking");
    expect(events[3]?.data.delta).toEqual({ type: "thinking_delta", thinking: "plan" });
    expect(events[6]?.data.delta).toEqual({ type: "text_delta", text: "done" });
    expect(events[8]?.data.usage.output_tokens).toBe(3);
  });
});

function jsonProtoClass(): ProtoClass {
  return {
    fromBinary(bytes: Uint8Array): ProtoMessage {
      return messageFromJson(JSON.parse(decoder.decode(bytes)));
    },
    fromJson(json: unknown): ProtoMessage {
      return messageFromJson(json);
    },
  };
}

function messageFromJson(json: unknown): ProtoMessage {
  return {
    toBinary(): Uint8Array {
      return jsonBytes(json);
    },
    toJson(): unknown {
      return json;
    },
  };
}

function frame(json: unknown): Uint8Array {
  return encodeConnectFrame(jsonBytes(json));
}

function jsonBytes(json: unknown): Uint8Array {
  return encoder.encode(JSON.stringify(json));
}

function streamFromChunks(chunks: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk);
      controller.close();
    },
  });
}
