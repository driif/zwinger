import { Accordion, Button, Card, CommandMenu, IconButton, SegmentedControl, Textarea } from "@driif/motiv";
import { ArrowUp, Bot, KeyRound, MessageSquarePlus, RotateCw, Square, TriangleAlert, Wrench } from "lucide-react";
import { KeyboardEvent, useEffect, useReducer, useRef, useState } from "react";
import { agentCall, AgentCommand, closeAgent, deleteChat, openAgent, ShieldView, ThinkingLevel } from "./api";
import type { ChatAction, TabView } from "./App";
import { Block, chatReducer, initialChat, Message, textOf, ToolRun } from "./agentChat";
import { Markdown } from "./Markdown";
import { reachable } from "./SecretsSection";
import { PROVIDER_KEYS } from "./providers";

interface Model {
  id: string;
  name: string;
  provider: string;
  reasoning: boolean;
}

interface AgentState {
  model?: Model;
  thinkingLevel: ThinkingLevel;
  sessionFile?: string;
}

interface Stats {
  tokens: { total: number };
  cost: number;
  contextUsage?: { percent: number | null };
}

const LEVELS: ThinkingLevel[] = ["off", "minimal", "low", "medium", "high"];

/** The values that tell one tool call from another. */
function summary(args: Record<string, unknown>) {
  const main = args.command ?? args.path ?? args.file_path ?? args.pattern ?? args.url;
  return typeof main === "string" ? main : JSON.stringify(args);
}

function ToolCard({ call, result, run }: { call: Extract<Block, { type: "toolCall" }>; result?: Message; run?: ToolRun }) {
  const status = result ? (result.isError ? "error" : "done") : (run?.status ?? "running");
  const output = result ? textOf(result.content) : (run?.output ?? "");
  return (
    <Accordion
      headingLevel={4}
      items={[
        {
          id: call.id,
          title: (
            <span className="row">
              <Wrench size={14} />
              <strong>{call.name}</strong>
              <span className="grow mono muted">{summary(call.arguments)}</span>
              <span className={status === "error" ? "danger" : "muted"}>{status}</span>
            </span>
          ),
          content: <pre className="tool-output mono">{output || "No output."}</pre>,
        },
      ]}
    />
  );
}

function MessageView({ message, results, tools }: { message: Message; results: Map<string, Message>; tools: Record<string, ToolRun> }) {
  if (message.role === "user")
    return (
      <div className="msg user">
        <Markdown text={textOf(message.content)} />
      </div>
    );
  if (message.role !== "assistant") return null;
  return (
    <div className="msg assistant">
      {(message.content as Block[]).map((block, i) => {
        if (block?.type === "text") return <Markdown key={i} text={block.text} />;
        if (block?.type === "thinking")
          return (
            <Accordion
              key={i}
              headingLevel={4}
              items={[{ id: "thinking", title: <span className="muted">Thinking</span>, content: <Markdown text={block.thinking} /> }]}
            />
          );
        if (block?.type === "toolCall")
          return <ToolCard key={block.id} call={block} result={results.get(block.id)} run={tools[block.id]} />;
        return null;
      })}
      {message.stopReason === "error" && <p className="danger">{message.errorMessage ?? "The model request failed."}</p>}
      {message.stopReason === "aborted" && <p className="muted">Stopped.</p>}
    </div>
  );
}

interface Props {
  shield: ShieldView;
  /** The chat that the tree selected. `undefined` keeps pi's current chat. */
  chat?: string;
  onJump: (tab: TabView) => void;
  /** Gets pi's current chat when it changes. */
  onChat: (chat: string) => void;
  /** The chat files may have changed. */
  onChatsChange: () => void;
  /** A rename or delete from the tree, which needs pi. */
  chatAction?: ChatAction;
  /** Gets the work of `chatAction` when it starts. */
  onChatActionDone: (work: Promise<unknown>) => void;
}

/** The chat with the shield's pi agent. pi runs in the shield, in its harness. */
export function AgentsTab({ shield, chat: wanted, onJump, onChat, onChatsChange, chatAction, onChatActionDone }: Props) {
  const running = shield.status.state === "running";
  const [agentId, setAgentId] = useState<string>();
  const [opens, setOpens] = useState(0);
  const [chat, dispatch] = useReducer(chatReducer, initialChat);
  const [state, setState] = useState<AgentState>();
  const [stats, setStats] = useState<Stats>();
  const [models, setModels] = useState<Model[]>([]);
  const [draft, setDraft] = useState("");
  const [mode, setMode] = useState<"steer" | "follow_up">("steer");
  const [error, setError] = useState<string>();
  const log = useRef<HTMLDivElement>(null);
  const loads = useRef(0);
  /** The agent that the chat shows. A load for an earlier agent is dropped. */
  const current = useRef<string | undefined>(undefined);
  const stick = useRef(true);

  const fail = (e: unknown) => setError(String(e));
  const reloadChats = onChatsChange;

  async function refresh(id: string) {
    const [next, nextStats] = await Promise.all([
      agentCall<AgentState>(id, { type: "get_state" }),
      agentCall<Stats>(id, { type: "get_session_stats" }),
    ]);
    setState(next);
    setStats(nextStats);
  }

  async function load(id: string) {
    if (id !== current.current) return;
    const load = ++loads.current;
    dispatch({ type: "loading", load });
    try {
      const [{ messages }, now] = await Promise.all([
        agentCall<{ messages: Message[] }>(id, { type: "get_messages" }),
        agentCall<{ isStreaming: boolean }>(id, { type: "get_state" }),
      ]);
      dispatch({ type: "base", load, messages, running: now.isStreaming });
    } catch (e) {
      dispatch({ type: "loadFailed", load });
      throw e;
    }
    await refresh(id);
  }

  useEffect(() => {
    reloadChats();
    if (!running) {
      setAgentId(undefined);
      return;
    }
    let live = true;
    let id: string | undefined;
    current.current = undefined;
    dispatch({ type: "reset" });
    openAgent(shield.id, (record) => {
      if (!live) return;
      dispatch({ type: "record", record });
      if (record.type === "agent_settled" && id) {
        refresh(id).catch(fail);
        reloadChats();
      }
    })
      .then(async (opened) => {
        if (!live) return;
        id = opened;
        current.current = opened;
        setAgentId(opened);
        await load(opened);
        setModels((await agentCall<{ models: Model[] }>(opened, { type: "get_available_models" })).models);
      })
      .catch((e) => live && fail(e));
    return () => {
      live = false;
    };
  }, [shield.id, running, opens]);

  useEffect(() => {
    if (stick.current) log.current?.scrollTo({ top: log.current.scrollHeight });
  }, [chat]);

  const ready = agentId !== undefined && chat.exit === undefined;
  const call = (command: AgentCommand) => {
    setError(undefined);
    return agentCall(agentId!, command);
  };
  const thenRefresh = (command: AgentCommand) => call(command).then(() => refresh(agentId!)).catch(fail);
  const thenLoad = (command: AgentCommand) =>
    call(command)
      .then(() => load(agentId!))
      .then(reloadChats)
      .catch(fail);

  function send() {
    const message = draft.trim();
    if (!message || !ready) return;
    setDraft("");
    stick.current = true;
    call({ type: chat.running ? mode : "prompt", message }).catch((e) => {
      setDraft(message);
      fail(e);
    });
  }

  function onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    send();
  }

  async function restart() {
    current.current = undefined;
    setAgentId(undefined);
    if (agentId) await closeAgent(agentId).catch(fail);
    setOpens((n) => n + 1);
  }

  const results = new Map(chat.messages.filter((m) => m.role === "toolResult").map((m) => [m.toolCallId!, m]));
  const shown = chat.partial ? [...chat.messages, chat.partial] : chat.messages;
  const currentChat = state?.sessionFile?.split("/").pop();

  useEffect(() => {
    if (currentChat) onChat(currentChat);
  }, [currentChat]);

  useEffect(() => {
    if (wanted && ready && !chat.running && wanted !== currentChat) thenLoad({ type: "switch_session", chat: wanted });
  }, [wanted, ready, chat.running]);

  // pi renames only its current chat, and must not delete the file that it writes.
  async function doChatAction(id: string, action: ChatAction) {
    const now = await agentCall<AgentState & { isStreaming: boolean }>(id, { type: "get_state" });
    if (now.isStreaming) throw "pi is busy. Wait until it is done, or stop it.";
    const holds = now.sessionFile?.split("/").pop() === action.chat;
    if (action.kind === "rename") {
      if (!holds) await agentCall(id, { type: "switch_session", chat: action.chat });
      await agentCall(id, { type: "set_session_name", name: action.name });
    } else {
      if (holds) await agentCall(id, { type: "new_session" });
      await deleteChat(shield.id, action.chat);
    }
    await load(id);
  }

  useEffect(() => {
    if (chatAction && ready) onChatActionDone(doChatAction(agentId, chatAction));
  }, [chatAction, ready]);

  const providerReady = shield.secrets.some(
    (s) => PROVIDER_KEYS.has(s.name) && s.hosts.some((h) => reachable(shield.network, h)),
  );
  const hosts =
    shield.network.mode === "public"
      ? "the public internet"
      : shield.network.mode === "deny" || shield.network.domains.length === 0
        ? "no hosts"
        : shield.network.domains.join(", ");

  return (
    <div className="agents">
      <section className="chat">
        <header className="chat-header row">
          <Button
            variant="outline"
            size="sm"
            iconLeft={<MessageSquarePlus size={14} />}
            disabled={!ready || chat.running}
            onClick={() => thenLoad({ type: "new_session" })}
          >
            New chat
          </Button>
          <CommandMenu
            className="model-picker"
            menuLabel="Models"
            placeholder={state?.model ? state.model.name : "Choose a model"}
            commands={models.map((m) => ({
              id: `${m.provider}/${m.id}`,
              label: `${m.name} · ${m.provider}`,
              onSelect: () => thenRefresh({ type: "set_model", provider: m.provider, modelId: m.id }),
            }))}
          />
          {state?.model?.reasoning && (
            <SegmentedControl
              label="Thinking level"
              options={LEVELS.map((level) => ({ value: level, label: level }))}
              value={state.thinkingLevel}
              onChange={(level) => thenRefresh({ type: "set_thinking_level", level: level as ThinkingLevel })}
            />
          )}
          <span className="grow muted">
            {stats &&
              `${stats.tokens.total.toLocaleString()} tokens · $${stats.cost.toFixed(2)}` +
                (stats.contextUsage?.percent != null ? ` · context ${Math.round(stats.contextUsage.percent)}%` : "")}
          </span>
          <IconButton icon={<RotateCw size={14} />} label="Restart agent" disabled={!running} onClick={restart} />
        </header>
        {!providerReady && (
          <Card variant="outlined" padding="sm" className="row">
            <KeyRound size={16} className="warn" />
            <span className="grow">
              No LLM provider is set up. Add a provider key, such as ANTHROPIC_API_KEY, as a secret, and allow its host.
            </span>
            <Button size="sm" variant="outline" onClick={() => onJump("network")}>
              Open Network
            </Button>
          </Card>
        )}
        <div
          ref={log}
          className="chat-log"
          onScroll={(e) => {
            const el = e.currentTarget;
            stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
          }}
        >
          {shown.length === 0 && (
            <div className="empty">
              <Bot size={32} className="muted" />
              <p>
                {running ? "pi" : "When the shield runs, pi"} works in <span className="mono">{shield.harness ?? "/workspace"}</span>.
              </p>
              <p className="muted">
                It sees <span className="mono">/workspace</span> (read-write)
                {shield.mounts.map((m) => (
                  <span key={m.guest}>
                    , <span className="mono">{m.guest}</span> ({m.readonly ? "read-only" : "read-write"})
                  </span>
                ))}
                . It reaches {hosts}.{" "}
                {shield.secrets.length > 0 ? `It gets placeholders for ${shield.secrets.map((s) => s.name).join(", ")}.` : "It has no secrets."}
              </p>
            </div>
          )}
          {shown.map((message, i) => (
            <MessageView key={i} message={message} results={results} tools={chat.tools} />
          ))}
          {chat.exit && running && (
            <Card variant="outlined" padding="sm" className="stack">
              <p className="danger row">
                <TriangleAlert size={14} />
                The agent ended with code {chat.exit.code}.
              </p>
              {chat.exit.stderr.length > 0 && <pre className="tool-output mono">{chat.exit.stderr.join("\n")}</pre>}
              <Button size="sm" variant="outline" onClick={restart}>
                Start the agent again
              </Button>
            </Card>
          )}
        </div>
        {chat.status && <p className="muted">{chat.status}</p>}
        {chat.queue.length > 0 && (
          <ul className="list muted">
            {chat.queue.map((text, i) => (
              <li key={i} className="grow">
                Waits: {text}
              </li>
            ))}
          </ul>
        )}
        {error && <p className="danger mono">{error}</p>}
        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <Textarea
            label="Message"
            hideLabel
            rows={1}
            maxRows={8}
            placeholder={!running ? "Start the shield to talk to pi" : chat.running ? "Send a message to pi while it works" : "Ask pi"}
            value={draft}
            disabled={!ready}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKeyDown}
          />
          <div className="row">
            {chat.running && (
              <SegmentedControl
                label="Deliver"
                options={[
                  { value: "steer", label: "Steer" },
                  { value: "follow_up", label: "Follow up" },
                ]}
                value={mode}
                onChange={(value) => setMode(value as "steer" | "follow_up")}
              />
            )}
            <span className="grow" />
            {chat.running && (
              <Button type="button" variant="secondary" iconLeft={<Square size={14} />} onClick={() => call({ type: "abort" }).catch(fail)}>
                Stop
              </Button>
            )}
            <Button type="submit" iconLeft={<ArrowUp size={14} />} disabled={!ready || draft.trim() === ""}>
              Send
            </Button>
          </div>
        </form>
      </section>
    </div>
  );
}
