import { Accordion, Badge, Button, Dialog, FloatingField, SpringCheckbox, Textarea } from "@driif/motiv";
import { GitCommitHorizontal, GitPullRequest } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { applyRunGit, GitKind, GitOperation, GitRequest, GitReview, reviewRunGit, runGitOperation, runGitUnfinished } from "./api";
import { unfinishedEffects } from "./gitOperation";
import { latestRequest } from "./diffRequest";
import { errorMessage } from "../../shared/errorMessage";

interface Props {
  sessionId: string;
  taskId: string;
  run: string;
}

const KIND_LABEL = { modified: "modified", added: "added", deleted: "deleted", renamed: "renamed", untracked: "new", unmerged: "conflict" };

function outcome(op: GitOperation) {
  switch (op.state) {
    case "committed":
      return `Committed ${op.sha}.`;
    case "pushed":
      return `Committed ${op.sha} and pushed it to ${op.ref}. The PR is not open yet.`;
    case "pr":
      return `Opened the PR ${op.url} with ${op.sha} on ${op.ref}.`;
    case "planned":
      return "The operation started, but its result is not known. Try again: the app checks what it already did.";
    case "stale":
      return `Not done: ${op.detail}.`;
    case "failed":
      return `The step ${op.step} failed: ${op.detail}`;
  }
}

/**
 * Commit or PR for a shipped run, after a review in the cage and an explicit approval.
 * The parent mounts it once per run, so a run switch discards all state.
 */
export function RunGitAction({ sessionId, taskId, run }: Props) {
  const [kind, setKind] = useState<GitKind>();
  const [selection, setSelection] = useState<string[] | null>(null);
  const [message, setMessage] = useState("");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [base, setBase] = useState("");
  const [again, setAgain] = useState(0);
  const [review, setReview] = useState<GitReview>();
  const [reviewing, setReviewing] = useState(false);
  const [applying, setApplying] = useState(false);
  const [result, setResult] = useState<GitOperation>();
  const [error, setError] = useState<string>();
  const [unfinished, setUnfinished] = useState<GitOperation[]>([]);
  const [continuing, setContinuing] = useState<string>();
  const [continued, setContinued] = useState<GitOperation>();
  const reviews = useRef(latestRequest()).current;
  const applies = useRef(latestRequest()).current;
  const lists = useRef(latestRequest()).current;
  const resumes = useRef(latestRequest()).current;

  // The journal survives a close, a run switch and a restart, so the room
  // lists what an earlier approval already did.
  useEffect(() => {
    const isCurrent = lists.next();
    runGitUnfinished(sessionId, taskId, run).then(
      (ops) => isCurrent() && setUnfinished(ops),
      () => {},
    );
  }, [kind, result, continued]);

  const resume = (op: GitOperation) => {
    const isCurrent = resumes.next();
    setContinuing(op.token);
    setContinued(undefined);
    // A recorded commit continues from the journal, not from a new review.
    const recorded: GitRequest = { kind: op.kind, selection: null, message: "", title: "", body: "", base: "" };
    applyRunGit(sessionId, taskId, run, recorded, op.token).then(
      (next) => {
        if (!isCurrent()) return;
        setContinued(next);
        setContinuing(undefined);
      },
      (e) => {
        if (!isCurrent()) return;
        setContinued({ ...op, state: "failed", step: op.step ?? "continue", detail: errorMessage(e) });
        setContinuing(undefined);
      },
    );
  };

  const request: GitRequest | undefined = kind && { kind, selection, message, title, body, base };
  const key = JSON.stringify(request ?? null);

  // Every change asks for a new review: the old approval does not count.
  useEffect(() => {
    setReview(undefined);
    setResult(undefined);
    setError(undefined);
    applies.drop();
    setApplying(false);
    if (!request) {
      reviews.drop();
      setReviewing(false);
      return;
    }
    const isCurrent = reviews.next();
    setReviewing(true);
    const timer = setTimeout(() => {
      reviewRunGit(sessionId, taskId, run, request).then(
        (found) => {
          if (!isCurrent()) return;
          setReview(found);
          setResult(found.operation ?? undefined);
          setReviewing(false);
        },
        (e) => {
          if (!isCurrent()) return;
          setError(errorMessage(e));
          setReviewing(false);
        },
      );
    }, 400);
    return () => clearTimeout(timer);
  }, [key, again]);

  useEffect(() => () => {
    reviews.drop();
    applies.drop();
    lists.drop();
    resumes.drop();
  }, []);

  const open = (next: GitKind) => {
    setKind(next);
    setSelection(null);
  };
  const close = () => setKind(undefined);

  const approve = () => {
    if (!request || !review?.token) return;
    const token = review.token;
    const isCurrent = applies.next();
    setApplying(true);
    setError(undefined);
    applyRunGit(sessionId, taskId, run, request, token).then(
      (op) => {
        if (!isCurrent()) return;
        setResult(op);
        setApplying(false);
      },
      (e) => {
        if (!isCurrent()) return;
        setError(errorMessage(e));
        setApplying(false);
        // The call can fail after a step ran: show what the journal recorded.
        runGitOperation(sessionId, taskId, run, token).then(
          (op) => isCurrent() && op && setResult(op),
          () => {},
        );
      },
    );
  };

  const selected = review?.files.filter((f) => f.selected).map((f) => f.path) ?? [];
  const toggle = (path: string, on: boolean) =>
    setSelection(on ? [...selected, path] : selected.filter((p) => p !== path));
  const done =
    result &&
    (result.state === "pr" || (result.state === "committed" && result.kind === "commit") || (result.state === "stale" && result.sha));
  const retry = result && (result.state === "failed" || result.state === "planned" || result.state === "pushed" || result.state === "committed");

  return (
    <section className="stack">
      <h2 className="room-heading">Commit or PR</h2>
      <p className="muted">
        Ship is the verdict of the agents. Nothing is committed or pushed until you review the change and approve it here.
      </p>
      <div className="row">
        <Button size="sm" variant="outline" iconLeft={<GitCommitHorizontal size={14} />} onClick={() => open("commit")}>
          Commit…
        </Button>
        <Button size="sm" variant="outline" iconLeft={<GitPullRequest size={14} />} onClick={() => open("pr")}>
          Open PR…
        </Button>
      </div>
      {unfinished.length > 0 && kind === undefined && (
        <ul className="list">
          {unfinished.map((op) => (
            <li key={op.token} className="row">
              <span>{unfinishedEffects(op)}</span>
              <Button size="sm" loading={continuing === op.token} disabled={continuing !== undefined} onClick={() => resume(op)}>
                Continue
              </Button>
            </li>
          ))}
        </ul>
      )}
      {continued && <p className={continued.state === "failed" || continued.state === "stale" ? "danger" : undefined}>{outcome(continued)}</p>}
      <Dialog open={kind !== undefined} title={kind === "pr" ? "Commit and open a PR" : "Commit"} size="wide" onClose={close}>
        <div className="stack">
          <Textarea label="Commit message" rows={3} maxRows={12} value={message} required onChange={(e) => setMessage(e.target.value)} />
          {kind === "pr" && (
            <>
              <FloatingField label="PR title (empty: the first line of the message)" value={title} showValidity={false} onChange={(e) => setTitle(e.target.value)} />
              <Textarea label="PR description" rows={2} maxRows={10} value={body} onChange={(e) => setBody(e.target.value)} />
              <FloatingField label="Base branch (empty: main)" value={base} showValidity={false} onChange={(e) => setBase(e.target.value)} />
            </>
          )}
          {reviewing && <p className="muted">Reviewing the change in the cage…</p>}
          {error && <p className="danger mono">{error}</p>}
          {review && (
            <>
              <ul className="list muted">
                <li>Runs {review.location}. {review.mount}.</li>
                <li>
                  Branch <span className="mono">{review.repo.branch.replace("refs/heads/", "")}</span> at{" "}
                  <span className="mono">{review.repo.head || "no commit"}</span>, git dir <span className="mono">{review.repo.gitdir}</span>.
                </li>
                <li>
                  Only files below <span className="mono">{review.scope}</span> count. The run's own records are left out.
                </li>
                {review.notes.map((n, i) => (
                  <li key={i}>{n}</li>
                ))}
              </ul>
              {review.files.length === 0 && <p className="muted">There is no change to commit.</p>}
              {review.files.map((f) => (
                <div key={f.path} className="stack">
                  <div className="row">
                    <SpringCheckbox
                      label={<span className="mono">{f.from ? `${f.from} → ${f.path}` : f.path}</span>}
                      checked={f.selected}
                      disabled={f.unselectable !== null}
                      title={f.unselectable ?? undefined}
                      onChange={(e) => toggle(f.path, e.target.checked)}
                    />
                    <Badge size="sm">{KIND_LABEL[f.kind]}</Badge>
                    {f.staged && <Badge size="sm">staged</Badge>}
                    {f.binary && <Badge size="sm" tone="danger">binary</Badge>}
                    {f.dirtyAtStart && <Badge size="sm" tone="warning">changed before the run</Badge>}
                  </div>
                  {f.selected && (
                    <Accordion
                      headingLevel={4}
                      items={[{ id: f.path, title: <span className="muted">Diff of {f.path}</span>, content: <pre className="tool-output mono room-diff">{f.diff || "No content change."}</pre> }]}
                    />
                  )}
                </div>
              ))}
              {review.pr && (
                <div className="stack">
                  <p>
                    Commit on your local branch <span className="mono">{review.repo.branch.replace("refs/heads/", "")}</span>: it moves
                    to the new commit. No local branch is created or switched. Then push the commit to{" "}
                    <span className="mono">{review.pr.head}</span> of origin
                    {review.pr.newBranch ? (
                      " (a new branch)"
                    ) : (
                      <>
                        {" "}
                        (the branch exists at <span className="mono">{review.pr.headTip}</span> and moves forward)
                      </>
                    )}
                    , never forced, then open a PR from it into <span className="mono">{review.pr.base}</span> of {review.pr.owner}/
                    {review.pr.repo}, with the title “{review.pr.title}”.
                  </p>
                  <p className="muted">
                    The PR has these commits on top of <span className="mono">{review.pr.baseTip}</span>:
                  </p>
                  <ul className="list mono">
                    {review.pr.commits.map((c, i) => (
                      <li key={i}>{c}</li>
                    ))}
                  </ul>
                  <Accordion
                    headingLevel={4}
                    items={[{ id: "pr", title: <span className="muted">The full diff of the PR</span>, content: <pre className="tool-output mono room-diff">{review.pr.diff || "No change."}</pre> }]}
                  />
                </div>
              )}
              {review.blocked.length > 0 && (
                <ul className="list">
                  {review.blocked.map((b, i) => (
                    <li key={i} className="danger">
                      {b[0].toUpperCase()}
                      {b.slice(1)}.
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}
          {result && <p className={result.state === "failed" || result.state === "stale" ? "danger" : undefined}>{outcome(result)}</p>}
          <div className="row end">
            <Button variant="ghost" onClick={close}>
              {done ? "Close" : "Cancel"}
            </Button>
            {result?.state === "stale" && !result.sha ? (
              <Button onClick={() => setAgain((n) => n + 1)}>Review again</Button>
            ) : (
              !done && (
                <Button disabled={!review?.token} loading={applying} onClick={approve}>
                  {retry ? "Try again" : kind === "pr" ? `Commit ${selected.length} files and open the PR` : `Commit ${selected.length} files`}
                </Button>
              )
            )}
          </div>
        </div>
      </Dialog>
    </section>
  );
}
