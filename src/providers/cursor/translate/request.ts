import type {
  AnthropicContentBlock,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicTextBlock,
  AnthropicToolResultContentBlock,
} from "../../../anthropic/schema.ts";

export function renderCursorPrompt(req: AnthropicRequest): string {
  const sections: string[] = [];
  const system = renderSystem(req.system);
  if (system) sections.push(`<system>\n${system}\n</system>`);

  for (const message of req.messages) {
    const content = renderContent(message);
    if (content) sections.push(`<${message.role}>\n${content}\n</${message.role}>`);
  }

  if (req.tools?.length) {
    sections.push(
      `<tools>\n${req.tools
        .map((tool) =>
          JSON.stringify({
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
          }),
        )
        .join("\n")}\n</tools>`,
    );
  }

  return sections.join("\n\n");
}

function renderSystem(system: AnthropicRequest["system"]): string | undefined {
  if (!system) return undefined;
  const blocks: AnthropicTextBlock[] =
    typeof system === "string" ? [{ type: "text", text: system }] : system;
  const text = blocks
    .filter((block) => block.type === "text" && typeof block.text === "string")
    .map((block) => block.text)
    .filter((line) => !line.startsWith("x-anthropic-billing-header:"))
    .join("\n\n");
  return text || undefined;
}

function renderContent(message: AnthropicMessage): string {
  const blocks: AnthropicContentBlock[] =
    typeof message.content === "string" ? [{ type: "text", text: message.content }] : message.content;
  return blocks.map(renderBlock).filter(Boolean).join("\n\n");
}

function renderBlock(block: AnthropicContentBlock): string {
  switch (block.type) {
    case "text":
      return block.text;
    case "thinking":
      return `<thinking>\n${block.thinking}\n</thinking>`;
    case "image":
      if (block.source.type === "url") return `[image: ${block.source.url}]`;
      return `[image: ${block.source.media_type}, ${block.source.data.length} base64 chars]`;
    case "tool_use":
      return `<tool_use id="${block.id}" name="${block.name}">\n${JSON.stringify(block.input ?? {})}\n</tool_use>`;
    case "tool_result":
      return `<tool_result tool_use_id="${block.tool_use_id}"${block.is_error ? " is_error=\"true\"" : ""}>\n${renderToolResult(block.content)}\n</tool_result>`;
  }
}

function renderToolResult(content: AnthropicToolResultContentBlock[] | string): string {
  if (typeof content === "string") return content;
  return content.map(renderToolResultBlock).filter(Boolean).join("\n\n");
}

function renderToolResultBlock(block: AnthropicToolResultContentBlock): string {
  if (block.type === "text" || block.type === "image" || block.type === "tool_use" || block.type === "tool_result" || block.type === "thinking") {
    return renderBlock(block as AnthropicContentBlock);
  }
  const type = typeof block.type === "string" ? block.type : "unknown";
  return `[unsupported tool result block: ${type}]`;
}
