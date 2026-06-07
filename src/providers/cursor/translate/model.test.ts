import { describe, expect, it } from "bun:test";
import { resolveCursorModel } from "./model.ts";

describe("Cursor model selection", () => {
  it("maps cursor aliases to composer fast", () => {
    const selected = resolveCursorModel({ model: "cursor", metadata: undefined });

    expect(selected.mode).toBe("AGENT_MODE_AGENT");
    expect(selected.requestedModel).toEqual({
      modelId: "composer-2.5",
      parameters: [{ id: "fast", value: "true" }],
    });
  });

  it("selects plan and ask modes from aliases", () => {
    expect(resolveCursorModel({ model: "cursor-plan", metadata: undefined }).mode).toBe(
      "AGENT_MODE_PLAN",
    );
    expect(resolveCursorModel({ model: "cursor-ask", metadata: undefined }).mode).toBe(
      "AGENT_MODE_ASK",
    );
  });

  it("supports raw cursor model prefix", () => {
    const selected = resolveCursorModel({
      model: "cursor:claude-sonnet-4-6-fast",
      metadata: { cursor_mode: "plan" },
    });

    expect(selected.mode).toBe("AGENT_MODE_PLAN");
    expect(selected.requestedModel).toEqual({
      modelId: "claude-sonnet-4-6",
      parameters: [{ id: "fast", value: "true" }],
    });
  });
});
