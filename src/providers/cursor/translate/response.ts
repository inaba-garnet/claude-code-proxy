import { encodeSseEvent } from "../../../sse.ts";
import type { Logger } from "../../../log.ts";
import type { TrafficCapture } from "../../types.ts";
import { decodeCursorStream, type CursorStreamEvent, type CursorUsage } from "../client.ts";
import type { CursorProto } from "../proto-loader.ts";

export interface AnthropicCursorResponse {
  id: string;
  type: "message";
  role: "assistant";
  model: string;
  content: Array<{ type: "thinking"; thinking: string; signature: string } | { type: "text"; text: string }>;
  stop_reason: "end_turn" | null;
  stop_sequence: null;
  usage: AnthropicUsage;
}

export interface AnthropicUsage {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
}

export function cursorUsageToAnthropic(usage?: CursorUsage): AnthropicUsage {
  const input = toNumber(usage?.inputTokens);
  const output = toNumber(usage?.outputTokens);
  const cacheRead = toNumber(usage?.cacheReadTokens);
  const cacheWrite = toNumber(usage?.cacheWriteTokens);
  return {
    input_tokens: Math.max(0, input - cacheRead - cacheWrite),
    output_tokens: output,
    cache_creation_input_tokens: cacheWrite,
    cache_read_input_tokens: cacheRead,
  };
}

export async function accumulateCursorResponse(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string;
    model: string;
    log: Logger;
    traffic?: TrafficCapture;
    proto?: CursorProto;
    onSession?: (sessionId: string) => void;
  },
): Promise<{ response: AnthropicCursorResponse; cursorSessionId?: string }> {
  let text = "";
  let thinking = "";
  let usage: CursorUsage | undefined;
  let cursorSessionId: string | undefined;

  for await (const event of decodeCursorStream(upstream, opts.proto)) {
    opts.traffic?.writeJsonEvent("040-cursor-event", event);
    switch (event.type) {
      case "session":
        cursorSessionId = event.sessionId;
        opts.onSession?.(event.sessionId);
        break;
      case "thinking_delta":
        thinking += event.text;
        break;
      case "text_delta":
        text += event.text;
        break;
      case "usage":
        usage = event.usage;
        break;
      case "end":
        break;
    }
  }

  const content: AnthropicCursorResponse["content"] = [];
  if (thinking) content.push({ type: "thinking", thinking, signature: "" });
  if (text) content.push({ type: "text", text });
  opts.log.debug("cursor accumulate finish", {
    textChars: text.length,
    thinkingChars: thinking.length,
    cursorSessionId,
    usage,
  });

  return {
    cursorSessionId,
    response: {
      id: opts.messageId,
      type: "message",
      role: "assistant",
      model: opts.model,
      content,
      stop_reason: "end_turn",
      stop_sequence: null,
      usage: cursorUsageToAnthropic(usage),
    },
  };
}

export function translateCursorStream(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string;
    model: string;
    log: Logger;
    signal?: AbortSignal;
    traffic?: TrafficCapture;
    proto?: CursorProto;
    onSession?: (sessionId: string) => void;
  },
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      let closed = false;
      let started = false;
      let thinkingOpen = false;
      let textOpen = false;
      let nextIndex = 0;
      let thinkingIndex = -1;
      let textIndex = -1;

      const emit = (event: string, data: unknown) => {
        if (closed || opts.signal?.aborted || controller.desiredSize === null) return false;
        opts.traffic?.writeJsonEvent("050-downstream-event", { event, data });
        controller.enqueue(encoder.encode(encodeSseEvent(event, data)));
        return true;
      };

      const ensureStart = () => {
        if (started) return;
        started = true;
        emit("message_start", {
          type: "message_start",
          message: {
            id: opts.messageId,
            type: "message",
            role: "assistant",
            model: opts.model,
            content: [],
            stop_reason: null,
            stop_sequence: null,
            usage: {
              input_tokens: 0,
              output_tokens: 0,
              cache_creation_input_tokens: 0,
              cache_read_input_tokens: 0,
            },
          },
        });
        emit("ping", { type: "ping" });
      };

      const openThinking = () => {
        if (thinkingOpen) return;
        ensureStart();
        thinkingOpen = true;
        thinkingIndex = nextIndex++;
        emit("content_block_start", {
          type: "content_block_start",
          index: thinkingIndex,
          content_block: { type: "thinking", thinking: "", signature: "" },
        });
      };

      const openText = () => {
        if (textOpen) return;
        ensureStart();
        textOpen = true;
        textIndex = nextIndex++;
        emit("content_block_start", {
          type: "content_block_start",
          index: textIndex,
          content_block: { type: "text", text: "" },
        });
      };

      const closeOpenBlocks = () => {
        if (thinkingOpen) {
          emit("content_block_stop", { type: "content_block_stop", index: thinkingIndex });
          thinkingOpen = false;
        }
        if (textOpen) {
          emit("content_block_stop", { type: "content_block_stop", index: textIndex });
          textOpen = false;
        }
      };

      try {
        let finalUsage: CursorUsage | undefined;
        for await (const event of decodeCursorStream(upstream, opts.proto)) {
          opts.traffic?.writeJsonEvent("040-cursor-event", event);
          if (opts.signal?.aborted) return;
          switch (event.type) {
            case "session":
              opts.onSession?.(event.sessionId);
              break;
            case "thinking_delta":
              openThinking();
              emit("content_block_delta", {
                type: "content_block_delta",
                index: thinkingIndex,
                delta: { type: "thinking_delta", thinking: event.text },
              });
              break;
            case "text_delta":
              if (thinkingOpen) {
                emit("content_block_stop", { type: "content_block_stop", index: thinkingIndex });
                thinkingOpen = false;
              }
              openText();
              emit("content_block_delta", {
                type: "content_block_delta",
                index: textIndex,
                delta: { type: "text_delta", text: event.text },
              });
              break;
            case "usage":
              finalUsage = event.usage;
              break;
            case "end":
              break;
          }
        }
        ensureStart();
        closeOpenBlocks();
        emit("message_delta", {
          type: "message_delta",
          delta: { stop_reason: "end_turn", stop_sequence: null },
          usage: cursorUsageToAnthropic(finalUsage),
        });
        emit("message_stop", { type: "message_stop" });
      } catch (err) {
        opts.log.warn("cursor stream error", { err: String(err) });
        ensureStart();
        closeOpenBlocks();
        emit("error", {
          type: "error",
          error: { type: "api_error", message: String(err) },
        });
      } finally {
        closed = true;
        try {
          controller.close();
        } catch {
          // Consumer cancellation can close the controller first.
        }
      }
    },
  });
}

function toNumber(value: string | undefined): number {
  if (!value) return 0;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : 0;
}
