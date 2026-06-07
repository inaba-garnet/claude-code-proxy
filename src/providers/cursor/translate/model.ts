import type { CursorAgentMode, CursorModelRequest } from "../client.ts";
import type { AnthropicRequest } from "../../../anthropic/schema.ts";

export interface CursorModelSelection {
  requestedModel: CursorModelRequest;
  mode: CursorAgentMode;
}

export const CURSOR_SUPPORTED_MODELS = new Set([
  "cursor",
  "cursor-agent",
  "cursor-composer",
  "cursor-composer-fast",
  "cursor-plan",
  "cursor-ask",
  "composer-2.5",
  "composer-2.5-fast",
]);

export function isCursorModel(model: string): boolean {
  return CURSOR_SUPPORTED_MODELS.has(model) || model.startsWith("cursor:");
}

export function resolveCursorModel(req: Pick<AnthropicRequest, "model" | "metadata">): CursorModelSelection {
  let mode = modeFromMetadata(req.metadata);
  if (req.model === "cursor-plan") mode = "AGENT_MODE_PLAN";
  else if (req.model === "cursor-ask") mode = "AGENT_MODE_ASK";
  else mode ??= "AGENT_MODE_AGENT";

  if (req.model.startsWith("cursor:")) {
    return { requestedModel: parseRawCursorModel(req.model.slice("cursor:".length)), mode };
  }

  switch (req.model) {
    case "cursor":
    case "cursor-agent":
    case "cursor-composer":
    case "cursor-composer-fast":
    case "cursor-plan":
    case "cursor-ask":
    case "composer-2.5-fast":
      return {
        requestedModel: { modelId: "composer-2.5", parameters: [{ id: "fast", value: "true" }] },
        mode,
      };
    case "composer-2.5":
      return { requestedModel: { modelId: "composer-2.5" }, mode };
    default:
      return { requestedModel: { modelId: req.model }, mode };
  }
}

function parseRawCursorModel(raw: string): CursorModelRequest {
  if (raw.endsWith("-fast")) {
    return {
      modelId: raw.slice(0, -"-fast".length),
      parameters: [{ id: "fast", value: "true" }],
    };
  }
  return { modelId: raw };
}

function modeFromMetadata(metadata: unknown): CursorAgentMode | undefined {
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return undefined;
  const value = (metadata as Record<string, unknown>).cursor_mode ?? (metadata as Record<string, unknown>).cursorMode;
  if (value === "plan" || value === "AGENT_MODE_PLAN") return "AGENT_MODE_PLAN";
  if (value === "ask" || value === "AGENT_MODE_ASK") return "AGENT_MODE_ASK";
  if (value === "agent" || value === "AGENT_MODE_AGENT") return "AGENT_MODE_AGENT";
  return undefined;
}
