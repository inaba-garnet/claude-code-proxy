import { describe, expect, it } from "bun:test";
import { renderCursorPrompt } from "./request.ts";
import type { AnthropicRequest } from "../../../anthropic/schema.ts";

describe("Cursor prompt rendering", () => {
  it("renders system, messages, tools, and tool results deterministically", () => {
    const req: AnthropicRequest = {
      model: "cursor",
      system: "Follow instructions.",
      messages: [
        { role: "user", content: "Question" },
        {
          role: "assistant",
          content: [
            { type: "text", text: "Calling tool" },
            { type: "tool_use", id: "toolu_1", name: "Read", input: { file: "package.json" } },
          ],
        },
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "toolu_1",
              content: [{ type: "text", text: "package content" }],
            },
          ],
        },
      ],
      tools: [{ name: "Read", input_schema: { type: "object" } }],
    };

    const prompt = renderCursorPrompt(req);

    expect(prompt).toContain("<system>\nFollow instructions.\n</system>");
    expect(prompt).toContain("<user>\nQuestion\n</user>");
    expect(prompt).toContain('<tool_use id="toolu_1" name="Read">');
    expect(prompt).toContain('<tool_result tool_use_id="toolu_1">');
    expect(prompt).toContain('"name":"Read"');
  });
});
