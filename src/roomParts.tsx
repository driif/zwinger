import { Accordion, Button, Card, CardBody, CardHeader, ProgressRing } from "@driif/motiv";
import {
  Activity,
  Bot,
  Braces,
  CircleCheck,
  CircleX,
  Download,
  Gem,
  LucideIcon,
  Sparkles,
  TriangleAlert,
  User,
  Wrench,
} from "lucide-react";
import { AGENT_NAMES, AgentKind, Blocking, cancelRuntimeInstall, Room, RoomMessage, RuntimeView } from "./api";
import type { Runtimes } from "./App";
import { Block, ChatState, Message, textOf, ToolRun } from "./agentChat";
import { Markdown } from "./Markdown";

export const AGENT_ICONS: Record<AgentKind, LucideIcon> = { pi: Bot, claude: Sparkles, codex: Braces, gemini: Gem };

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

/** In-app runs name agents by kind; duo rounds use their own names. */
function iconOf(name: string): LucideIcon {
  if (name === "user") return User;
  if (name === "run") return Activity;
  return AGENT_ICONS[name as AgentKind] ?? Bot;
}

export function AgentName({ name }: { name: string }) {
  const Icon = iconOf(name);
  return (
    <span className="row room-agent">
      <Icon size={14} />
      {name}
    </span>
  );
}

const time = (at: string | null) => (at ? new Date(at).toLocaleTimeString() : "");

export function elapsed(ms: number) {
  const secs = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, "0")}`;
}

/** The messages of the round, in order. Each one opens to its full text. */
export function Timeline({ room }: { room: Room }) {
  const label = (id: string) => room.stages.find((s) => s.id === id)?.label ?? id;
  const key = (m: RoomMessage) => `${m.stage}|${m.kind}|${m.from}|${m.at}|${m.summary}`;
  return (
    <Accordion
      multiple
      headingLevel={3}
      items={room.messages.map((m) => ({
        id: key(m),
        title: (
          <span className={`row room-message ${m.kind === "event" ? "muted" : ""}`}>
            <AgentName name={m.from} />
            {m.to !== "all" && <span className="muted">→ {m.to}</span>}
            <span className="room-chip">{label(m.stage)}</span>
            <span className="grow room-summary">{m.summary}</span>
            <span className="muted">{time(m.at)}</span>
          </span>
        ),
        content: <Markdown text={m.body} />,
      }))}
    />
  );
}

/** The streamed output of one agent in the active stage. */
export function LiveEntry({ agent, chat, since }: { agent: string; chat?: ChatState; since: string }) {
  const messages = chat?.messages ?? [];
  const results = new Map<string, Message>(
    messages.filter((m) => m.role === "toolResult" && m.toolCallId).map((m) => [m.toolCallId!, m]),
  );
  const shown = [...messages.filter((m) => m.role === "assistant").slice(-3), ...(chat?.partial ? [chat.partial] : [])];
  return (
    <Card variant="subtle" padding="sm" className="stack">
      <p className="row">
        <AgentName name={agent} />
        <span className="muted grow">working… {since}</span>
      </p>
      {shown.map((message, i) => (
        <MessageView key={i} message={message} results={results} tools={chat?.tools ?? {}} />
      ))}
    </Card>
  );
}

/** What each agent wants to build now, side by side. */
export function Positions({ room }: { room: Room }) {
  if (room.positions.length === 0) return null;
  return (
    <section className="stack">
      <h2 className="room-heading">Positions</h2>
      <div className="room-positions">
        {room.positions.map((p) => (
          <Card key={p.agent} variant="outlined" padding="sm" className="stack">
            <p className="row">
              <AgentName name={p.agent} />
              <span className="muted grow">
                {p.source} · wants {p.implementer ?? "?"} to implement
              </span>
            </p>
            <Markdown text={p.plan} />
          </Card>
        ))}
      </div>
    </section>
  );
}

/** Where the agents differed, who conceded what, and how it ended. */
export function Disagreements({ room }: { room: Room }) {
  if (room.disagreements.length === 0 && room.concessions.length === 0) return null;
  const converged = room.turns.find((t) => t.converged);
  return (
    <section className="stack">
      <h2 className="room-heading">Disagreements</h2>
      <ul className="list">
        {room.disagreements.map((d, i) => (
          <li key={i} className={d.resolved ? "row" : "row warn"}>
            {d.resolved ? <CircleCheck size={14} className="ok" /> : <TriangleAlert size={14} />}
            <span className="grow">
              Turn {d.turn}: {d.detail}
            </span>
            <span className="muted">{d.resolved ? `resolved: ${d.resolved}` : "open"}</span>
          </li>
        ))}
      </ul>
      {converged && (
        <p className="muted">
          Both agreed in turn {converged.turn} on {converged.converged}.
        </p>
      )}
      {room.concessions.length > 0 && (
        <ul className="list muted">
          {room.concessions.map((c, i) => (
            <li key={i}>
              Turn {c.turn}, {c.agent} conceded: {c.point}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

const DECIDED = {
  agreement: "by agreement",
  tiebreak: "by tiebreak, not by agreement",
  walkover: "by walkover: the other agent made no plan. This is not an agreement",
  user: "picked by the user",
};

/** The decision, the reviews and the outcome. Degraded agents are marked, never shown as agreement. */
export function Verdicts({ room }: { room: Room }) {
  const { crossreview, outcome } = room;
  return (
    <section className="stack">
      <h2 className="room-heading">Verdicts</h2>
      {room.implementer && (
        <p>
          <strong>{room.implementer}</strong> implements{room.decidedBy && `, ${DECIDED[room.decidedBy]}`}.
          {room.tiebreak && <span className="muted"> {room.tiebreak.why}</span>}
        </p>
      )}
      {room.degraded.length > 0 && (
        <Card variant="outlined" padding="sm" className="stack">
          <p className="row warn">
            <TriangleAlert size={14} />
            Degraded
          </p>
          <ul className="list">
            {room.degraded.map((d, i) => (
              <li key={i}>
                {d.stage}: {d.detail}
              </li>
            ))}
          </ul>
        </Card>
      )}
      {room.violations.length > 0 && (
        <Card variant="outlined" padding="sm" className="stack">
          <p className="row danger">
            <TriangleAlert size={14} />
            Changes in read-only stages
          </p>
          <ul className="list">
            {room.violations.map((v, i) => (
              <li key={i}>{v.detail}</li>
            ))}
          </ul>
        </Card>
      )}
      {crossreview && (
        <Card variant="outlined" padding="sm">
          <CardHeader
            titleAs="h4"
            title={`Cross review by ${crossreview.reviewer ?? "?"}: ${crossreview.verdict}`}
            description={crossreview.message}
          />
          {crossreview.findings.length > 0 && (
            <CardBody>
              <ul className="list">
                {crossreview.findings.map((f, i) => (
                  <li key={i} className={f.severity === "blocking" ? "danger" : undefined}>
                    [{f.severity}]{" "}
                    <span className="mono">
                      {f.file}
                      {f.line !== null && `:${f.line}`}
                    </span>{" "}
                    — {f.what}
                  </li>
                ))}
              </ul>
            </CardBody>
          )}
        </Card>
      )}
      {room.finals.map((f) => (
        <Card key={f.agent} variant="outlined" padding="sm" className="stack">
          <p className="row">
            {f.ship ? <CircleCheck size={14} className="ok" /> : <CircleX size={14} className="danger" />}
            <AgentName name={f.agent} />
            <strong className="grow">{f.ship ? "ship" : "no ship"}</strong>
          </p>
          <p>{f.taskMet}</p>
          <Blockings items={f.blocking} />
          {f.residual.length > 0 && (
            <ul className="list muted">
              {f.residual.map((r, i) => (
                <li key={i}>{r}</li>
              ))}
            </ul>
          )}
        </Card>
      ))}
      {outcome && (
        <Card variant="elevated" padding="sm" className="stack">
          <p className="row">
            {outcome.ship ? <CircleCheck size={16} className="ok" /> : <CircleX size={16} className="danger" />}
            <strong className="grow">{outcome.ship ? "Ship" : "No ship"}</strong>
            <span className="muted">{outcome.stopped ?? outcome.shipBasis}</span>
          </p>
          <Blockings items={outcome.blocking} />
          {outcome.backlog.length > 0 && (
            <>
              <p className="muted">Backlog</p>
              <ul className="list">
                {outcome.backlog.map((b, i) => (
                  <li key={i}>{b}</li>
                ))}
              </ul>
            </>
          )}
        </Card>
      )}
    </section>
  );
}

function Blockings({ items }: { items: Blocking[] }) {
  if (items.length === 0) return null;
  return (
    <ul className="list">
      {items.map((b, i) => (
        <li key={i} className="danger">
          {b.what} <span className="mono">{b.where}</span>
          <span className="muted"> — {b.why}</span>
        </li>
      ))}
    </ul>
  );
}

export function Gates({ room }: { room: Room }) {
  if (room.gates.length === 0) return null;
  return (
    <section className="stack">
      <h2 className="room-heading">Gates</h2>
      <Accordion
        multiple
        headingLevel={4}
        items={room.gates.map((g, i) => ({
          id: String(i),
          title: (
            <span className="row">
              {g.code === 0 ? <CircleCheck size={14} className="ok" /> : <CircleX size={14} className="danger" />}
              <span className="mono grow">{g.command}</span>
              <span className="muted">{g.code === null ? "no exit" : `exit ${g.code}`}</span>
            </span>
          ),
          content: <pre className="tool-output mono">{g.output || "No output."}</pre>,
        }))}
      />
    </section>
  );
}
