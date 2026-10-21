import {
  Accordion,
  Button,
  Card,
  CommandMenu,
  FloatingField,
  IconButton,
  ProgressRing,
  SegmentedControl,
  Textarea,
} from "@driif/motiv";
import {
  ArrowUp,
  Bot,
  Braces,
  Download,
  KeyRound,
  LucideIcon,
  RotateCw,
  Sparkles,
  Square,
  TriangleAlert,
  Wrench,
} from "lucide-react";
import { KeyboardEvent, useEffect, useReducer, useRef, useState } from "react";
import {
  AGENT_NAMES,
  agentCall,
  AgentCommand,
  AgentKind,
  cancelRuntimeInstall,
  closeAgent,
  deleteChat,
  openAgent,
  RuntimeName,
  RuntimeView,
  ShieldView,
  ThinkingLevel,
} from "./api";
import type { ChatAction, Runtimes, TabView } from "./App";
import { Block, chatReducer, initialChat, Message, textOf, ToolRun } from "./agentChat";
import { Markdown } from "./Markdown";
import { reachable } from "./SecretsSection";
import { AGENT_KEYS, PROVIDER_KEYS } from "./providers";

export const AGENT_ICONS: Record<AgentKind, LucideIcon> = { pi: Bot, claude: Sparkles, codex: Braces };

const KINDS: AgentKind[] = ["pi", "claude", "codex"];

/** Suggestions only: the model field takes any name that the CLI knows. */
const MODEL_PRESETS: Record<RuntimeName, string[]> = {
  claude: ["sonnet", "opus", "haiku"],
  codex: ["gpt-5-codex", "gpt-5"],
};

/** Install, or the progress of the install that runs. Only the download can be cancelled. */
export function InstallControl({ runtime, runtimes }: { runtime: RuntimeView; runtimes: Runtimes }) {
  const progress = runtimes.installs[runtime.name];
  const name = AGENT_NAMES[runtime.name];
  if (!progress)
    return (
      <Button size="sm" variant="outline" iconLeft={<Download size={14} />} onClick={() => runtimes.onInstall(runtime.name)}>
        Install
      </Button>
    );
  if (progress.stage === "extract") return <span className="muted">Checking and unpacking {name}…</span>;
  const mb = Math.round(progress.received / 1e6);
  return (
    <span className="row">
      {progress.total ? (
        <>
          <ProgressRing value={Math.min(99, Math.floor((progress.received / progress.total) * 100))} label={`${name} download`} size={32} />
          <span className="muted">
            {mb} of {Math.round(progress.total / 1e6)} MB
          </span>
        </>
      ) : (
        <span className="muted">Downloading {name}: {mb} MB</span>
      )}
      <Button size="sm" variant="ghost" onClick={() => cancelRuntimeInstall(runtime.name)}>
        Cancel
      </Button>
    </span>
  );
}

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

export function MessageView({ message, results, tools }: { message: Message; results: Map<string, Message>; tools: Record<string, ToolRun> }) {
  if (message.role === "user")
    return (
      <div className="msg user">
        <Markdown text={textOf(message.content)} />
      </div>
    );
  if (message.role === "notice") return <p className="muted">{textOf(message.content)}</p>;
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
  agent: AgentKind;
  /** The chat of `agent` that the tree selected. `undefined` keeps the agent's current chat. */
  chat?: string;
  /** Shows the chat of another agent. */
  onAgent: (agent: AgentKind) => void;
  runtimes: Runtimes;
  onRestart: () => void;
  onJump: (tab: TabView) => void;
  /** Gets the agent's current chat when it changes; `undefined` after a new chat that has no message yet. */
  onChat: (agent: AgentKind, chat?: string) => void;
  /** The chat files may have changed. */
  onChatsChange: () => void;
  /** A rename or delete of a pi chat from the tree, which needs pi. */
  chatAction?: ChatAction;
  /** Gets the work of `chatAction` when it starts. */
  onChatActionDone: (work: Promise<unknown>) => void;
}

/** The chat with an agent of the shield. The agent runs in the shield, in its harness. */
export function AgentsTab({
  shield,
  agent,
  chat: wanted,
  onAgent,
  runtimes,
  onRestart,
  onJump,
  onChat,
  onChatsChange,
  chatAction,
  onChatActionDone,
}: Props) {
  const running = shield.status.state === "running";
  const isPi = agent === "pi";
  const name = AGENT_NAMES[agent];
  const Icon = AGENT_ICONS[agent];
  const runtime = isPi ? undefined : runtimes.list.find((r) => r.name === agent);
  const installed = isPi || runtime?.installed === true;
  const mounted = isPi || shield.runtimes.includes(agent);
  const [opened, setOpened] = useState<{ kind: AgentKind; id: string }>();
  // The tab stays mounted across agents: the ID of another agent is no ID.
  const agentId = opened?.kind === agent ? opened.id : undefined;
  const [opens, setOpens] = useState(0);
  const [chat, dispatch] = useReducer(chatReducer, initialChat);
  const [state, setState] = useState<AgentState>();
  const [stats, setStats] = useState<Stats>();
  const [models, setModels] = useState<Model[]>([]);
  const [draft, setDraft] = useState("");
  const [model, setModel] = useState("");
  const [mode, setMode] = useState<"steer" | "follow_up">("steer");
  const [error, setError] = useState<string>();
  const log = useRef<HTMLDivElement>(null);
  const loads = useRef(0);
  /** The agent that the chat shows. A load for an earlier agent is dropped. */
  const current = useRef<string | undefined>(undefined);
  const stick = useRef(true);
  /** The agent whose chat shows. Another agent starts with an empty chat. */
  const shownAgent = useRef(agent);
  /** The agent that gets a new chat when it is open. */
  const newFor = useRef<AgentKind | undefined>(undefined);

  const fail = (e: unknown) => setError(String(e));
  const reloadChats = onChatsChange;

  // Claude Code and Codex have no stats, model list or thinking levels.
  async function refresh(id: string) {
    const [next, nextStats] = await Promise.all([
      agentCall<AgentState>(id, { type: "get_state" }),
      isPi ? agentCall<Stats>(id, { type: "get_session_stats" }) : undefined,
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
    if (shownAgent.current !== agent) {
      shownAgent.current = agent;
      current.current = undefined;
      dispatch({ type: "reset" });
      setState(undefined);
      setStats(undefined);
      setModels([]);
    }
    if (!running || !mounted) {
      setOpened(undefined);
      return;
    }
    let live = true;
    let id: string | undefined;
    current.current = undefined;
    dispatch({ type: "reset" });
    openAgent(shield.id, agent, (record) => {
      if (!live) return;
      dispatch({ type: "record", record });
      if (record.type === "agent_settled" && id) {
        refresh(id).catch(fail);
        reloadChats();
      }
      // The current chat was deleted.
      if (record.type === "chat_reset" && id) {
        onChat(agent, undefined);
        load(id).catch(fail);
      }
    })
      .then(async (opened) => {
        if (!live) return;
        id = opened;
        current.current = opened;
        setOpened({ kind: agent, id: opened });
        await load(opened);
        if (newFor.current === agent) {
          newFor.current = undefined;
          await agentCall(opened, { type: "new_session" });
          onChat(agent, undefined);
          await load(opened);
        }
        if (isPi) setModels((await agentCall<{ models: Model[] }>(opened, { type: "get_available_models" })).models);
      })
      .catch((e) => live && fail(e));
    return () => {
      live = false;
    };
  }, [shield.id, running, opens, agent, mounted]);

  useEffect(() => setModel(state?.model?.id ?? ""), [state?.model?.id]);

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

  // Claude Code and Codex take no message while they work.
  const canSend = ready && (isPi || !chat.running);

  function send() {
    const message = draft.trim();
    if (!message || !canSend) return;
    setDraft("");
    stick.current = true;
    call({ type: chat.running ? mode : "prompt", message }).then(
      // Their chat file starts with the first message.
      () => {
        if (!isPi) {
          reloadChats();
          refresh(agentId!).catch(fail);
        }
      },
      (e) => {
        setDraft(message);
        fail(e);
      },
    );
  }

  function newChat(kind: AgentKind) {
    if (kind !== agent) {
      newFor.current = kind;
      onAgent(kind);
    } else if (ready && !chat.running)
      call({ type: "new_session" })
        .then(() => {
          onChat(agent, undefined);
          return load(agentId!);
        })
        .then(reloadChats)
        .catch(fail);
  }

  function onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    send();
  }

  async function restart() {
    current.current = undefined;
    setOpened(undefined);
    if (agentId) await closeAgent(agentId).catch(fail);
    setOpens((n) => n + 1);
  }

  const results = new Map(chat.messages.filter((m) => m.role === "toolResult").map((m) => [m.toolCallId!, m]));
  const shown = chat.partial ? [...chat.messages, chat.partial] : chat.messages;
  const currentChat = state?.sessionFile?.split("/").pop();

  useEffect(() => {
    if (currentChat) onChat(agent, currentChat);
  }, [currentChat, agent]);

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
      await deleteChat(shield.id, "pi", action.chat);
    }
    await load(id);
  }

  useEffect(() => {
    if (chatAction && ready && isPi) onChatActionDone(doChatAction(agentId, chatAction));
  }, [chatAction, ready, isPi]);

  const keys = isPi ? PROVIDER_KEYS : new Set(AGENT_KEYS[agent]);
  const providerReady = shield.secrets.some((s) => keys.has(s.name) && s.hosts.some((h) => reachable(shield.network, h)));
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
          <CommandMenu
            className="model-picker"
            menuLabel="New chat"
            placeholder="New chat"
            commands={KINDS.map((kind) => ({
              id: kind,
              label: `New ${AGENT_NAMES[kind]} chat`,
              onSelect: () => newChat(kind),
            }))}
          />
          <span className="row" title={name}>
            <Icon size={16} aria-hidden />
            <strong>{name}</strong>
          </span>
          {isPi ? (
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
          ) : (
            <form
              className="row"
              onSubmit={(e) => {
                e.preventDefault();
                thenRefresh({ type: "set_model", provider: agent, modelId: model });
              }}
            >
              <FloatingField
                label="Model"
                className="model-picker mono"
                value={model}
                hint={model ? undefined : "Empty uses the default model"}
                showValidity={false}
                disabled={!ready || chat.running}
                onChange={(e) => setModel(e.target.value)}
                onBlur={() => model !== (state?.model?.id ?? "") && thenRefresh({ type: "set_model", provider: agent, modelId: model })}
              />
              <CommandMenu
                className="model-picker"
                menuLabel="Model presets"
                placeholder="Presets"
                commands={MODEL_PRESETS[agent].map((preset) => ({
                  id: preset,
                  label: preset,
                  onSelect: () => {
                    setModel(preset);
                    thenRefresh({ type: "set_model", provider: agent, modelId: preset });
                  },
                }))}
              />
            </form>
          )}
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
        {runtime && !installed && (
          <Card variant="outlined" padding="sm" className="row">
            <Download size={16} className="warn" />
            <span className="grow">
              {name} is not installed. zwinger downloads version {runtime.version} from npm, checks it and mounts it
              read-only in the shields.
            </span>
            <InstallControl runtime={runtime} runtimes={runtimes} />
          </Card>
        )}
        {installed && running && !mounted && (
          <Card variant="outlined" padding="sm" className="row">
            <RotateCw size={16} className="warn" />
            <span className="grow">{name} was installed after the shield started. Restart the shield to use it.</span>
            <Button size="sm" variant="outline" onClick={onRestart}>
              Restart
            </Button>
          </Card>
        )}
        {!providerReady && (
          <Card variant="outlined" padding="sm" className="row">
            <KeyRound size={16} className="warn" />
            <span className="grow">
              {isPi
                ? "No LLM provider is set up. Add a provider key, such as ANTHROPIC_API_KEY, as a secret, and allow its host."
                : `No key for ${name} is set up. Add ${AGENT_KEYS[agent].join(" or ")} as a secret, and allow its host.`}
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
                {running ? name : `When the shield runs, ${name}`} works in{" "}
                <span className="mono">{shield.harness ?? "/workspace"}</span>.
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
            placeholder={
              !running
                ? `Start the shield to talk to ${name}`
                : chat.running
                  ? isPi
                    ? "Send a message to pi while it works"
                    : `${name} works. Wait, or stop it.`
                  : `Ask ${name}`
            }
            value={draft}
            disabled={!canSend}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKeyDown}
          />
          <div className="row">
            {chat.running && isPi && (
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
            <Button type="submit" iconLeft={<ArrowUp size={14} />} disabled={!canSend || draft.trim() === ""}>
              Send
            </Button>
          </div>
        </form>
      </section>
    </div>
  );
}
