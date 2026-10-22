import type { AgentRecord } from "./api";

/** The pi content blocks that the chat shows. */
export type Block =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string }
  | { type: "toolCall"; id: string; name: string; arguments: Record<string, unknown> }
  | { type: "image" };

/** A pi `AgentMessage`, with the fields that the chat reads. */
export interface Message {
  role: string;
  content?: string | Block[];
  toolCallId?: string;
  isError?: boolean;
  stopReason?: string;
  errorMessage?: string;
  timestamp: number;
}

export interface ToolRun {
  status: "running" | "done" | "error";
  output: string;
}

export interface ChatState {
  messages: Message[];
  /** The assistant message that streams now. */
  partial?: Message;
  /** Live tool runs by tool call id, until their result message arrives. */
  tools: Record<string, ToolRun>;
  running: boolean;
  /** Steering and follow-up messages that wait. */
  queue: string[];
  /** Retry or compaction in progress. */
  status?: string;
  exit?: { code: number; stderr: string[] };
  /** The load that waits for its base. Records that arrive meanwhile wait in `buffer`. */
  load?: number;
  buffer: AgentRecord[];
}

export type ChatAction =
  | { type: "reset" }
  | { type: "loading"; load: number }
  | { type: "base"; load: number; messages: Message[]; running: boolean }
  /** The load failed: the waiting records apply to the current state. */
  | { type: "loadFailed"; load: number }
  | { type: "record"; record: AgentRecord };

export const initialChat: ChatState = { messages: [], tools: {}, running: false, queue: [], buffer: [] };

export function textOf(content: Message["content"]) {
  if (typeof content === "string") return content;
  return (content ?? []).map((b) => (b.type === "text" ? b.text : "")).join("");
}

interface StreamEvent {
  type: string;
  contentIndex: number;
  delta?: string;
  content?: string;
  id?: string;
  toolName?: string;
  toolCall?: Block;
}

/** Applies one `message_update` delta. Blocks are keyed by `contentIndex`; an `_end` replaces the block. */
function stream(partial: Message | undefined, event: StreamEvent): Message {
  // The tab can attach in the middle of a message, after its message_start.
  const message = partial ?? { role: "assistant", content: [], timestamp: Date.now() };
  const content = [...(message.content as Block[])];
  const i = event.contentIndex;
  const block = content[i];
  switch (event.type) {
    case "text_start":
    case "text_delta":
    case "text_end": {
      const text = block?.type === "text" ? block.text : "";
      content[i] = { type: "text", text: event.content ?? text + (event.delta ?? "") };
      break;
    }
    case "thinking_start":
    case "thinking_delta":
    case "thinking_end": {
      const thinking = block?.type === "thinking" ? block.thinking : "";
      content[i] = { type: "thinking", thinking: event.content ?? thinking + (event.delta ?? "") };
      break;
    }
    case "toolcall_start":
      content[i] = { type: "toolCall", id: event.id!, name: event.toolName!, arguments: {} };
      break;
    case "toolcall_end":
      content[i] = event.toolCall!;
      break;
    default:
      return message;
  }
  return { ...message, content };
}

const toolOutput = (result: unknown) => textOf((result as { content?: Block[] } | undefined)?.content);

/**
 * `get_messages` gives the base; the records after it are deltas. Records that
 * arrive during a load are applied after its base, and only the latest load
 * counts. Unknown records leave the state as it is, so a newer pi does not
 * break the chat.
 */
export function chatReducer(state: ChatState, action: ChatAction): ChatState {
  switch (action.type) {
    case "reset":
      return initialChat;
    case "loading":
      return { ...state, load: action.load, buffer: [] };
    case "base": {
      if (action.load !== state.load) return state;
      const base: ChatState = {
        ...state,
        messages: action.messages,
        running: action.running,
        partial: action.running ? state.partial : undefined,
        load: undefined,
        buffer: [],
      };
      return state.buffer.reduce(apply, base);
    }
    case "loadFailed":
      return action.load === state.load ? state.buffer.reduce(apply, { ...state, load: undefined, buffer: [] }) : state;
    case "record":
      return state.load === undefined ? apply(state, action.record) : { ...state, buffer: [...state.buffer, action.record] };
  }
}

const same = (a: Message, b: Message) => a.role === b.role && a.timestamp === b.timestamp;

function apply(state: ChatState, record: AgentRecord): ChatState {
  const r = record as Record<string, any>;
  switch (r.type) {
    case "agent_start":
      return { ...state, running: true, exit: undefined };
    case "agent_settled":
      return { ...state, running: false, partial: undefined, status: undefined };
    case "message_start":
      return r.message.role === "assistant" ? { ...state, partial: { ...r.message, content: [] } } : state;
    case "message_update":
      return { ...state, partial: stream(state.partial, r.assistantMessageEvent) };
    case "message_end":
      return {
        ...state,
        // A record from before the base can hold a message that the base has.
        messages: state.messages.some((m) => same(m, r.message)) ? state.messages : [...state.messages, r.message],
        partial: r.message.role === "assistant" ? undefined : state.partial,
      };
    case "tool_execution_start":
      return { ...state, tools: { ...state.tools, [r.toolCallId]: { status: "running", output: "" } } };
    case "tool_execution_update":
      return { ...state, tools: { ...state.tools, [r.toolCallId]: { status: "running", output: toolOutput(r.partialResult) } } };
    case "tool_execution_end":
      return {
        ...state,
        tools: { ...state.tools, [r.toolCallId]: { status: r.isError ? "error" : "done", output: toolOutput(r.result) } },
      };
    case "queue_update":
      return { ...state, queue: [...r.steering, ...r.followUp] };
    case "compaction_start":
      return { ...state, status: "Compacting the context…" };
    case "auto_retry_start":
      return { ...state, status: `Retry ${r.attempt} of ${r.maxAttempts}: ${r.errorMessage}` };
    case "status":
      return { ...state, status: r.message };
    case "compaction_end":
    case "auto_retry_end":
      return { ...state, status: undefined };
    case "agent_exit":
      return { ...state, running: false, partial: undefined, status: undefined, exit: { code: r.code, stderr: r.stderr } };
    default:
      return state;
  }
}
