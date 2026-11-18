import assert from "node:assert/strict";
import { test } from "node:test";
import xterm from "@xterm/xterm";
import { followTitle, terminalTitles } from "./titles.ts";

const write = (term: InstanceType<typeof xterm.Terminal>, data: string) =>
  new Promise<void>((resolve) => term.write(data, resolve));

test("OSC 0 and OSC 2 set the title of their process; an empty title removes it", async () => {
  const term = new xterm.Terminal({ allowProposedApi: true });
  const unfollow = followTitle(term, "p1");
  await write(term, "\x1b]0;vim notes.md\x07");
  assert.equal(terminalTitles().p1, "vim notes.md");
  const before = terminalTitles();
  await write(term, "\x1b]2;~/src: zsh\x1b\\");
  assert.equal(terminalTitles().p1, "~/src: zsh");
  assert.notEqual(terminalTitles(), before, "a new title is a new snapshot");
  await write(term, "\x1b]2;   \x07");
  assert.equal(terminalTitles().p1, undefined);
  await write(term, "\x1b]0;again\x07");
  await write(term, "\x1b]0;\x07");
  assert.equal(terminalTitles().p1, undefined);
  unfollow();
  term.dispose();
});

test("each process keeps its own title, and the title ends with the process", async () => {
  const a = new xterm.Terminal({ allowProposedApi: true });
  const b = new xterm.Terminal({ allowProposedApi: true });
  const stopA = followTitle(a, "a");
  const stopB = followTitle(b, "b");
  await write(a, "\x1b]0;htop\x07");
  await write(b, "\x1b]2;claude\x07");
  assert.deepEqual(terminalTitles(), { a: "htop", b: "claude" });

  stopA();
  assert.deepEqual(terminalTitles(), { b: "claude" });
  // Output after the end does not bring the title back.
  await write(a, "\x1b]0;stale\x07");
  assert.deepEqual(terminalTitles(), { b: "claude" });

  // A restart is a new process id: it starts with no title.
  const restarted = new xterm.Terminal({ allowProposedApi: true });
  const stopRestarted = followTitle(restarted, "a2");
  await write(restarted, "plain output\r\n");
  assert.equal(terminalTitles().a2, undefined);
  stopRestarted();
  stopB();
  assert.deepEqual(terminalTitles(), {});
  for (const term of [a, b, restarted]) term.dispose();
});

test("an end can run twice and keeps the title of a newer binding of the same process", async () => {
  const old = new xterm.Terminal({ allowProposedApi: true });
  const other = new xterm.Terminal({ allowProposedApi: true });
  const stopOld = followTitle(old, "p");
  const stopOther = followTitle(other, "q");
  await write(other, "\x1b]0;other\x07");
  await write(old, "\x1b]0;old\x07");

  // A new view of the same process, for example after a remount, takes the title over.
  const current = new xterm.Terminal({ allowProposedApi: true });
  const stopCurrent = followTitle(current, "p");
  await write(old, "\x1b]0;from the old view\x07");
  assert.equal(terminalTitles().p, "old");
  await write(current, "\x1b]0;current\x07");
  stopOld();
  stopOld();
  assert.deepEqual(terminalTitles(), { p: "current", q: "other" });

  stopCurrent();
  stopCurrent();
  assert.deepEqual(terminalTitles(), { q: "other" });
  stopOther();
  for (const term of [old, other, current]) term.dispose();
});

test("an unchanged title does not publish a new snapshot", async () => {
  const term = new xterm.Terminal({ allowProposedApi: true });
  const stop = followTitle(term, "s");
  await write(term, "\x1b]2;same\x07");
  const before = terminalTitles();
  await write(term, "\x1b]0;same \x07");
  assert.equal(terminalTitles(), before);
  stop();
  const ended = terminalTitles();
  stop();
  assert.equal(terminalTitles(), ended);
  term.dispose();
});
