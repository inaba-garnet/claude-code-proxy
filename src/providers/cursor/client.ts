import { gunzipSync } from "node:zlib";
import http2 from "node:http2";
import { cursorBaseUrl, cursorClientVersion } from "../../config.ts";
import type { RequestContext } from "../types.ts";
import type { CursorProto } from "./proto-loader.ts";
import { loadCursorProto } from "./proto-loader.ts";
import type { CursorAuth } from "./auth/token-store.ts";

export interface CursorRunOptions {
  prompt: string;
  mode: CursorAgentMode;
  conversationId: string;
  model: CursorModelRequest;
  auth: CursorAuth;
  ctx: RequestContext;
  proto?: CursorProto;
  openRunStream?: CursorRunStreamFactory;
}

export type CursorRunStreamFactory = (opts: {
  requestId: string;
  accessToken: string;
  ctx: RequestContext;
}) => Promise<CursorRunStream>;

export interface CursorRunStream {
  readable: ReadableStream<Uint8Array>;
  status: Promise<{ status: number; detail?: string }>;
  write(frame: Uint8Array): Promise<void>;
  close(): void;
}

export type CursorAgentMode = "AGENT_MODE_AGENT" | "AGENT_MODE_PLAN" | "AGENT_MODE_ASK";

export interface CursorModelRequest {
  modelId: string;
  parameters?: Array<{ id: string; value: string }>;
}

export interface CursorUsage {
  inputTokens: string;
  outputTokens: string;
  cacheReadTokens?: string;
  cacheWriteTokens?: string;
}

export type CursorStreamEvent =
  | { type: "session"; sessionId: string }
  | { type: "thinking_delta"; text: string }
  | { type: "text_delta"; text: string }
  | { type: "usage"; usage: CursorUsage }
  | { type: "end" };

export class CursorError extends Error {
  constructor(
    public status: number,
    message: string,
    public detail?: string,
  ) {
    super(message);
    this.name = "CursorError";
  }
}

const HEARTBEAT_INTERVAL_MS = 5_000;

export async function runCursorAgent(opts: CursorRunOptions): Promise<ReadableStream<Uint8Array>> {
  const proto = opts.proto ?? loadCursorProto();
  const requestId = crypto.randomUUID();
  const openRunStream = opts.openRunStream ?? openHttp2RunStream;
  const runUrl = `${cursorBaseUrl().replace(/\/$/, "")}/agent.v1.AgentService/Run`;

  opts.ctx.traffic?.writeJson("020-cursor-run-request", {
    url: runUrl,
    requestId,
    conversationId: opts.conversationId,
    mode: opts.mode,
    model: opts.model,
  });

  const runStream = await openRunStream({
    requestId,
    accessToken: opts.auth.accessToken,
    ctx: opts.ctx,
  });

  let appendQueue = Promise.resolve();
  const append = async (messageJson: unknown) => {
    appendQueue = appendQueue.then(async () => {
      const messageBytes = proto.AgentClientMessage.fromJson(messageJson).toBinary();
      const frame = encodeConnectFrame(messageBytes);
      opts.ctx.traffic?.writeBytes("021-cursor-run-frame", frame);
      await runStream.write(frame);
    });
    await appendQueue;
  };

  await append({
    runRequest: {
      conversationState: {},
      action: {
        userMessageAction: {
          userMessage: {
            text: opts.prompt,
            messageId: crypto.randomUUID(),
            selectedContext: {},
            mode: opts.mode,
          },
        },
      },
      mcpTools: {},
      conversationId: opts.conversationId,
      requestedModel: opts.model,
      excludeWorkspaceContext: false,
      selectedSubagentModels: selectedSubagentModels(opts.model),
      conversationGroupId: opts.conversationId,
    },
  });

  const heartbeat = setInterval(() => {
    append({ clientHeartbeat: {} }).catch((err) => {
      opts.ctx.childLogger("cursor.client").warn("cursor heartbeat failed", {
        err: String(err),
      });
    });
  }, HEARTBEAT_INTERVAL_MS);

  const abort = () => {
    clearInterval(heartbeat);
    runStream.close();
  };
  opts.ctx.signal.addEventListener("abort", abort, { once: true });

  const response = await runStream.status;
  if (response.status < 200 || response.status >= 300) {
    abort();
    throw new CursorError(response.status, `Cursor AgentService/Run failed with HTTP ${response.status}`, response.detail);
  }

  return runStream.readable.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      async transform(chunk, controller) {
        controller.enqueue(chunk);
        await processServerControlFrames(chunk, proto, append);
      },
      flush() {
        clearInterval(heartbeat);
        opts.ctx.signal.removeEventListener("abort", abort);
        runStream.close();
      },
    }),
  );
}

export async function openHttp2RunStream(opts: {
  requestId: string;
  accessToken: string;
  ctx: RequestContext;
}): Promise<CursorRunStream> {
  const base = new URL(cursorBaseUrl());
  const session = http2.connect(base.origin);
  await new Promise<void>((resolve, reject) => {
    const cleanup = () => {
      session.off("connect", onConnect);
      session.off("error", onError);
    };
    const onConnect = () => {
      cleanup();
      resolve();
    };
    const onError = (err: Error) => {
      cleanup();
      reject(err);
    };
    session.once("connect", onConnect);
    session.once("error", onError);
  });

  let closed = false;
  let responseStatus = 0;
  let errorDetail = "";
  let resolveStatus!: (value: { status: number; detail?: string }) => void;
  let rejectStatus!: (err: Error) => void;
  const status = new Promise<{ status: number; detail?: string }>((resolve, reject) => {
    resolveStatus = resolve;
    rejectStatus = reject;
  });

  const stream = session.request({
    ":method": "POST",
    ":path": `${base.pathname.replace(/\/$/, "")}/agent.v1.AgentService/Run`,
    "authorization": `Bearer ${opts.accessToken}`,
    "content-type": "application/connect+proto",
    "connect-protocol-version": "1",
    "connect-accept-encoding": "gzip,br",
    "user-agent": "connect-es/1.6.1",
    "x-cursor-client-type": "cli",
    "x-cursor-client-version": cursorClientVersion(),
    "x-ghost-mode": "true",
    "x-request-id": opts.requestId,
    "x-original-request-id": opts.requestId,
    "x-cursor-streaming": "true",
    "te": "trailers",
  });

  stream.once("response", (headers) => {
    responseStatus = Number(headers[":status"] ?? 0);
    if (responseStatus >= 200 && responseStatus < 300) {
      resolveStatus({ status: responseStatus });
    }
  });

  const readable = new ReadableStream<Uint8Array>({
    start(controller) {
      stream.on("data", (chunk: Buffer) => {
        if (responseStatus >= 400) {
          errorDetail += chunk.toString("utf8");
          return;
        }
        controller.enqueue(new Uint8Array(chunk));
      });
      stream.once("end", () => {
        if (responseStatus >= 400) {
          resolveStatus({ status: responseStatus, detail: errorDetail || undefined });
        }
        controller.close();
        session.close();
      });
      stream.once("error", (err) => {
        rejectStatus(err instanceof Error ? err : new Error(String(err)));
        controller.error(err);
        session.destroy();
      });
      session.once("error", (err) => {
        rejectStatus(err instanceof Error ? err : new Error(String(err)));
        controller.error(err);
      });
    },
    cancel() {
      close();
    },
  });

  const close = () => {
    if (closed) return;
    closed = true;
    stream.close();
    session.close();
  };

  return {
    readable,
    status,
    write(frame: Uint8Array) {
      if (closed || stream.destroyed) {
        return Promise.reject(new Error("Cursor HTTP/2 Run stream is closed"));
      }
      return new Promise<void>((resolve, reject) => {
        stream.write(Buffer.from(frame), (err) => {
          if (err) reject(err);
          else resolve();
        });
      });
    },
    close,
  };
}

async function processServerControlFrames(
  chunk: Uint8Array,
  proto: CursorProto,
  append: (messageJson: unknown) => Promise<void>,
): Promise<void> {
  const state = controlFrameState.get(append) ?? {
    buffer: Buffer.alloc(0),
    execHeartbeatSent: false,
    requestContextAcked: false,
  };
  controlFrameState.set(append, state);
  state.buffer = Buffer.concat([state.buffer, Buffer.from(chunk)]);
  while (state.buffer.byteLength >= 5) {
    const flags = state.buffer[0]!;
    const len = state.buffer.readUInt32BE(1);
    if (state.buffer.byteLength < 5 + len) break;
    let payload = state.buffer.subarray(5, 5 + len);
    state.buffer = state.buffer.subarray(5 + len);
    if (flags & 1) payload = gunzipSync(payload);
    if (flags & 2) continue;
    const message = proto.AgentServerMessage.fromBinary(payload) as unknown as CursorOneofMessage;
    const oneof = message.message;
    if (oneof?.case === "execServerMessage") {
      if (!state.execHeartbeatSent) {
        state.execHeartbeatSent = true;
        await append({ execClientControlMessage: { heartbeat: {} } });
      }
      if (oneof.value?.message?.case === "requestContextArgs" && !state.requestContextAcked) {
        state.requestContextAcked = true;
        await append({ execClientMessage: buildRequestContextResult(oneof.value) });
        await append({ execClientControlMessage: { streamClose: {} } });
      }
    } else if (oneof?.case === "kvServerMessage") {
      const kv = oneof.value;
      if (kv?.message?.case === "setBlobArgs") {
        const msg: Record<string, unknown> = { setBlobResult: {} };
        if (typeof kv.id === "number" && kv.id !== 0) msg.id = kv.id;
        await append({ kvClientMessage: msg });
      } else if (kv?.message?.case === "getBlobArgs") {
        const msg: Record<string, unknown> = { getBlobResult: {} };
        if (typeof kv.id === "number" && kv.id !== 0) msg.id = kv.id;
        await append({ kvClientMessage: msg });
      }
    }
  }
}

function buildRequestContextResult(exec: {
  id?: number;
  execId?: string;
  message?: { case?: string; value?: unknown };
}): Record<string, unknown> {
  return {
    ...(typeof exec.id === "number" ? { id: exec.id } : {}),
    ...(typeof exec.execId === "string" ? { execId: exec.execId } : {}),
    requestContextResult: {
      success: {
        requestContext: {
          env: {
            osVersion: `${process.platform} ${process.arch}`,
            workspacePaths: [process.cwd()],
            shell: process.env.SHELL || "",
            sandboxEnabled: false,
            projectFolder: process.cwd(),
            processWorkingDirectory: process.cwd(),
          },
          repositoryInfoComplete: true,
          rulesInfoComplete: true,
          envInfoComplete: true,
          customSubagentsInfoComplete: true,
          mcpFileSystemInfoComplete: true,
          mcpInfoComplete: true,
          gitStatusInfoComplete: true,
          agentSkillsInfoComplete: true,
        },
      },
    },
  };
}

const controlFrameState = new WeakMap<
  (messageJson: unknown) => Promise<void>,
  { buffer: Buffer; execHeartbeatSent: boolean; requestContextAcked: boolean }
>();

interface CursorOneofMessage {
  message?: {
    case?: string;
    value?: {
      id?: number;
      execId?: string;
      message?: { case?: string; value?: unknown };
    };
  };
}

export async function* decodeCursorStream(
  body: ReadableStream<Uint8Array>,
  proto: CursorProto = loadCursorProto(),
): AsyncGenerator<CursorStreamEvent> {
  let buffer = Buffer.alloc(0);
  const reader = body.getReader();
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      if (!value?.byteLength) continue;
      buffer = Buffer.concat([buffer, Buffer.from(value)]);
      while (buffer.byteLength >= 5) {
        const flags = buffer[0]!;
        const len = buffer.readUInt32BE(1);
        if (buffer.byteLength < 5 + len) break;
        let payload = buffer.subarray(5, 5 + len);
        buffer = buffer.subarray(5 + len);
        if (flags & 1) payload = gunzipSync(payload);
        if (flags & 2) {
          yield { type: "end" };
          continue;
        }
        const decoded = safeToJson(proto.AgentServerMessage.fromBinary(payload));
        if (!decoded) continue;
        yield* eventsFromServerMessage(decoded);
      }
    }
  } finally {
    reader.releaseLock();
  }
}

function* eventsFromServerMessage(json: unknown): Generator<CursorStreamEvent> {
  if (!isRecord(json)) return;
  const exec = asRecord(json.execServerMessage);
  const requestContextArgs = asRecord(exec?.requestContextArgs);
  const notesSessionId = requestContextArgs?.notesSessionId;
  if (typeof notesSessionId === "string" && notesSessionId) {
    yield { type: "session", sessionId: notesSessionId };
  }

  const interaction = asRecord(json.interactionUpdate);
  const thinkingDelta = asRecord(interaction?.thinkingDelta)?.text;
  if (typeof thinkingDelta === "string" && thinkingDelta) {
    yield { type: "thinking_delta", text: thinkingDelta };
  }
  const textDelta = asRecord(interaction?.textDelta)?.text;
  if (typeof textDelta === "string" && textDelta) {
    yield { type: "text_delta", text: textDelta };
  }
  const turnEnded = asRecord(interaction?.turnEnded);
  if (turnEnded) {
    yield {
      type: "usage",
      usage: {
        inputTokens: stringToken(turnEnded.inputTokens),
        outputTokens: stringToken(turnEnded.outputTokens),
        cacheReadTokens: stringToken(turnEnded.cacheReadTokens),
        cacheWriteTokens: stringToken(turnEnded.cacheWriteTokens),
      },
    };
  }
}

export function encodeConnectFrame(payload: Uint8Array, flags = 0): Uint8Array {
  const out = new Uint8Array(5 + payload.byteLength);
  const view = new DataView(out.buffer);
  view.setUint8(0, flags);
  view.setUint32(1, payload.byteLength, false);
  out.set(payload, 5);
  return out;
}

function selectedSubagentModels(model: CursorModelRequest): CursorModelRequest[] {
  return [
    { modelId: "default" },
    model,
    {
      modelId: "claude-opus-4-8",
      parameters: [
        { id: "thinking", value: "true" },
        { id: "context", value: "300k" },
        { id: "effort", value: "high" },
        { id: "fast", value: "false" },
      ],
    },
    {
      modelId: "gpt-5.5",
      parameters: [
        { id: "context", value: "272k" },
        { id: "reasoning", value: "extra-high" },
        { id: "fast", value: "false" },
      ],
    },
    {
      modelId: "claude-sonnet-4-6",
      parameters: [
        { id: "thinking", value: "true" },
        { id: "context", value: "200k" },
        { id: "effort", value: "medium" },
      ],
    },
    {
      modelId: "gpt-5.3-codex",
      parameters: [
        { id: "reasoning", value: "medium" },
        { id: "fast", value: "false" },
      ],
    },
  ];
}

function safeToJson(message: { toJson(options?: unknown): unknown }): unknown | undefined {
  try {
    return message.toJson({ emitDefaultValues: false });
  } catch {
    return undefined;
  }
}

function stringToken(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number") return String(value);
  return "0";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}
