import { describe, expect, it } from "bun:test";
import { encodeConnectFrame, runCursorAgent } from "./client.ts";
import type { CursorProto, ProtoClass, ProtoMessage } from "./proto-loader.ts";
import type { RequestContext } from "../types.ts";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

describe("Cursor protocol client", () => {
  it("acks exec setup and KV messages on the HTTP/2 Run stream", async () => {
    const sentFrames: Uint8Array[] = [];

    const upstream = await runCursorAgent({
      prompt: "hello",
      mode: "AGENT_MODE_AGENT",
      conversationId: "conversation",
      model: { modelId: "composer-2.5" },
      auth: { accessToken: "token", source: "test" },
      ctx: fakeCtx(),
      proto: fakeProto,
      openRunStream: async () => ({
        readable: streamFromChunks([
          frame({
            message: {
              case: "execServerMessage",
              value: { id: 0, message: { case: "requestContextArgs", value: {} } },
            },
          }),
          frame({
            message: {
              case: "kvServerMessage",
              value: { id: 0, message: { case: "setBlobArgs", value: {} } },
            },
          }),
          frame({
            message: {
              case: "kvServerMessage",
              value: { id: 2, message: { case: "getBlobArgs", value: {} } },
            },
          }),
          encodeConnectFrame(jsonBytes({}), 2),
        ]),
        status: Promise.resolve({ status: 200 }),
        async write(frame) {
          sentFrames.push(frame);
        },
        close() {},
      }),
    });
    await drain(upstream);

    const clientMessages = sentFrames.map(decodeFrameJson) as Array<Record<string, any>>;
    expect(clientMessages[0]?.runRequest.conversationId).toBe("conversation");
    expect(clientMessages[1]).toEqual({ execClientControlMessage: { heartbeat: {} } });
    expect(clientMessages[2]?.execClientMessage.requestContextResult.success.requestContext).toBeDefined();
    expect(clientMessages[3]).toEqual({ execClientControlMessage: { streamClose: {} } });
    expect(clientMessages[4]).toEqual({ kvClientMessage: { setBlobResult: {} } });
    expect(clientMessages[5]).toEqual({ kvClientMessage: { getBlobResult: {}, id: 2 } });
  });
});

const fakeProto: CursorProto = {
  AgentServerMessage: jsonProtoClass(),
  AgentClientMessage: jsonProtoClass(),
};

function jsonProtoClass(): ProtoClass {
  return {
    fromBinary(bytes: Uint8Array): ProtoMessage {
      const json = JSON.parse(decoder.decode(bytes));
      return messageFromJson(json);
    },
    fromJson(json: unknown): ProtoMessage {
      return messageFromJson(json);
    },
  };
}

function messageFromJson(json: unknown): ProtoMessage {
  return Object.assign(
    {
      toBinary(): Uint8Array {
        return jsonBytes(json);
      },
      toJson(): unknown {
        return json;
      },
    },
    json && typeof json === "object" && !Array.isArray(json) ? json : {},
  );
}

function frame(json: unknown): Uint8Array {
  return encodeConnectFrame(jsonBytes(json));
}

function jsonBytes(json: unknown): Uint8Array {
  return encoder.encode(JSON.stringify(json));
}

function decodeFrameJson(frame: Uint8Array): unknown {
  const buf = Buffer.from(frame);
  const len = buf.readUInt32BE(1);
  return JSON.parse(decoder.decode(buf.subarray(5, 5 + len)));
}

function streamFromChunks(chunks: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk);
      controller.close();
    },
  });
}

async function drain(stream: ReadableStream<Uint8Array>): Promise<void> {
  const reader = stream.getReader();
  try {
    while (!(await reader.read()).done) {
      // Drain.
    }
  } finally {
    reader.releaseLock();
  }
}

function fakeCtx(): RequestContext {
  return {
    reqId: "req",
    signal: new AbortController().signal,
    childLogger: () => ({
      debug() {},
      info() {},
      warn() {},
      error() {},
      child() {
        return this;
      },
    }),
  };
}
