import { encode } from "gpt-tokenizer/model/gpt-4o";
import type { AnthropicRequest } from "../../anthropic/schema.ts";
import { renderCursorPrompt } from "./translate/request.ts";

export function countCursorTokens(req: AnthropicRequest): number {
  let total = encode(renderCursorPrompt(req)).length;
  for (const tool of req.tools ?? []) {
    total += encode(tool.name).length;
    if (tool.description) total += encode(tool.description).length;
    total += encode(JSON.stringify(tool.input_schema ?? {})).length;
  }
  return total;
}
