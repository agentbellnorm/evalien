import Anthropic from "@anthropic-ai/sdk";
import type {
  MessageParam,
  TextBlockParam,
} from "@anthropic-ai/sdk/resources/messages.mjs";
import { debug } from "./util.mts";

export function createClient(): Anthropic {
  return new Anthropic();
}

export interface AgentEval {
  eval: string;
}

export interface AgentBadResponse {
  raw: string;
}

export type AgentResponse = AgentEval | AgentBadResponse | null;

export async function callAgent(
  client: Anthropic,
  systemPrompt: string,
  messages: MessageParam[],
): Promise<AgentResponse> {
  const t0 = Date.now();

  const system: TextBlockParam[] = [
    { type: "text", text: systemPrompt, cache_control: { type: "ephemeral" } },
  ];

  // Mark a stable interior point for prompt caching — a few turns back from
  // the end. New messages only appear at the tail (2 per tick: agent + result),
  // so everything before that is identical to the previous request.
  const tagged = [...messages];
  const cacheIdx = Math.max(0, tagged.length - 4);
  if (tagged.length > 1) {
    const msg = tagged[cacheIdx];
    const cached: TextBlockParam = {
      type: "text",
      text: String(msg.content),
      cache_control: { type: "ephemeral" },
    };
    tagged[cacheIdx] = { ...msg, content: [cached] };
  }

  const response = await client.messages.create({
    model: "claude-sonnet-4-6",
    max_tokens: 4096,
    system,
    messages: tagged,
  });

  const { cache_read_input_tokens = 0, input_tokens = 0 } = response.usage;
  debug(
    `response in ${Date.now() - t0}ms | tokens: ${input_tokens} in, ${cache_read_input_tokens} cached`,
  );

  const block = response.content[0];
  const text = block && "text" in block ? block.text : undefined;
  if (!text) return null;

  return parseAgentResponse(text) ?? { raw: text };
}

function parseAgentResponse(text: string): AgentEval | null {
  const trimmed = text.trim();
  try {
    const parsed = JSON.parse(trimmed);
    if (parsed.eval !== undefined) return parsed;
  } catch {}

  const braceMatch = trimmed.match(/\{[\s\S]*?"eval"[\s\S]*?\}/);
  if (braceMatch) {
    try {
      const parsed = JSON.parse(braceMatch[0]);
      if (parsed.eval !== undefined) return parsed;
    } catch {}
  }

  return null;
}
