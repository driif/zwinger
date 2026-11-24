use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::Instant;

use crate::cage::manager::{AgentSink, FolderSink, State, lock};
use crate::runtime::{PtyEvent, TerminalKind};
use crate::terminal::{AgentState, FRESH_NOTICE, QUICK_EXIT, TerminalFolder, resolve_folder, starts_fresh};

/// The output bytes kept per session, for a view that attaches later.
pub(crate) const RING: usize = 1 << 20;

/// A frame takes the queued output up to this many bytes, or for at most
/// [`FRAME_WAIT`].
pub(crate) const FRAME: usize = 64 << 10;

pub(crate) const FRAME_WAIT: Duration = Duration::from_millis(8);

/// A first chunk up to this size with nothing queued after it is sent at
/// once, so the echo of a key has no delay.
pub(crate) const ECHO: usize = 1 << 10;

/// The frames that a view may have not acknowledged. Then the session stops
/// reading its process until the view acknowledges one.
pub(crate) const WINDOW: usize = 8;

/// A view that acknowledges no frame for this long is gone, such as after a
/// reload, and is detached.
pub(crate) const ACK_LIMIT: Duration = Duration::from_secs(5);

/// The kept output of a session and the view that gets new output.
#[derive(Default)]
pub(crate) struct Output {
    pub(crate) ring: VecDeque<u8>,
    pub(crate) exit: Option<i32>,
    pub(crate) sink: Option<Sink>,
    pub(crate) attaches: u64,
    /// Wakes the pump when a view acknowledges a frame or goes.
    pub(crate) room: Arc<Notify>,
    pub(crate) osc: Osc,
    /// The last folder of this process, which the listing shows: its start
    /// folder until the first report. `None` after the exit, or while unknown.
    pub(crate) folder: Option<TerminalFolder>,
    /// Goes up at each new OSC 7 folder and at each change of `folder`. A
    /// folder read at an older value is stale.
    pub(crate) folder_seq: u64,
}

/// The longest OSC 7 body that the scanner keeps across frames.
const OSC7_LIMIT: usize = 4 << 10;

const OSC7: &[u8] = b"\x1b]7;";

/// The sequence that an agent hook writes to its terminal, followed by its
/// state and BEL or `ESC \`.
pub(crate) const AGENT_OSC: &[u8] = b"\x1b]1337;zwinger-agent=";

/// The longest agent state value. A longer value is no agent sequence.
const AGENT_VALUE_LIMIT: usize = 16;

/// Where the scanner is in the output.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
enum Scan {
    #[default]
    Text,
    /// The held bytes are this many bytes of [`AGENT_OSC`].
    Agent(usize),
    /// The held bytes are the agent sequence and its value so far.
    AgentValue,
    /// This many bytes of [`OSC7`] went out.
    Osc7(usize),
    Osc7Body,
}

/// Reads the OSC sequences of the output. A sequence can span frames.
///
/// - OSC 7 (`ESC ] 7 ; file://host/path`): the current folder that a shell
///   reports. It goes out unchanged.
/// - [`AGENT_OSC`]: the state of the agent. It never goes out. The scanner
///   holds back a tail that can start it until the next frame shows the rest.
///
/// Both end with BEL or `ESC \`.
#[derive(Default)]
pub(crate) struct Osc {
    scan: Scan,
    /// The bytes that may be an agent sequence, or the ESC that may end an
    /// OSC 7 body.
    held: Vec<u8>,
    body: Vec<u8>,
    /// The OSC 7 body had an ESC, which starts the `ESC \` end.
    esc: bool,
    /// The last reported folder: a guest path for a VM shell.
    pub(crate) cwd: Option<String>,
    pub(crate) agent: Option<AgentState>,
}

impl Osc {
    /// Returns the bytes that go out: the held bytes that are no agent
    /// sequence, then `bytes` without the agent sequences.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.held.len() + bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if self.scan == Scan::Text {
                let Some(at) = bytes[i..].iter().position(|&b| b == 0x1b) else {
                    out.extend_from_slice(&bytes[i..]);
                    break;
                };
                out.extend_from_slice(&bytes[i..i + at]);
                i += at + 1;
                self.held.push(0x1b);
                self.scan = Scan::Agent(1);
                continue;
            }
            let b = bytes[i];
            i += 1;
            if !self.step(b, &mut out) {
                // `b` ended no sequence: it can start one.
                i -= 1;
            }
        }
        out
    }

    /// The held bytes, which no later output can make an agent sequence:
    /// the process has exited.
    pub(crate) fn release(&mut self) -> Vec<u8> {
        self.reset();
        std::mem::take(&mut self.held)
    }

    /// Takes `b` in a sequence. False: `b` is not taken and goes to the text.
    fn step(&mut self, b: u8, out: &mut Vec<u8>) -> bool {
        match self.scan {
            Scan::Text => unreachable!("the text is scanned in bulk"),
            Scan::Agent(2) if b == OSC7[2] => {
                out.append(&mut self.held);
                out.push(b);
                self.scan = Scan::Osc7(3);
            }
            Scan::Agent(n) if b == AGENT_OSC[n] => {
                self.held.push(b);
                self.scan = if n + 1 == AGENT_OSC.len() { Scan::AgentValue } else { Scan::Agent(n + 1) };
            }
            Scan::Agent(_) => {
                out.append(&mut self.held);
                self.scan = Scan::Text;
                return false;
            }
            Scan::AgentValue => match b {
                0x07 => self.finish_agent(),
                b'\\' if self.held.last() == Some(&0x1b) => self.finish_agent(),
                // The ESC was no end: drop the sequence, and the ESC may start a new one.
                _ if self.held.last() == Some(&0x1b) => {
                    self.held.clear();
                    self.held.push(0x1b);
                    self.scan = Scan::Agent(1);
                    return false;
                }
                _ if self.held.len() - AGENT_OSC.len() >= AGENT_VALUE_LIMIT => {
                    out.append(&mut self.held);
                    self.scan = Scan::Text;
                    return false;
                }
                _ => self.held.push(b),
            },
            Scan::Osc7(n) if b == OSC7[n] => {
                out.push(b);
                self.scan = if n + 1 == OSC7.len() { Scan::Osc7Body } else { Scan::Osc7(n + 1) };
            }
            Scan::Osc7(_) => {
                self.scan = Scan::Text;
                return false;
            }
            Scan::Osc7Body if self.esc => {
                self.esc = false;
                if b == b'\\' {
                    out.append(&mut self.held);
                    out.push(b);
                    self.finish_osc7();
                } else {
                    // The ESC was no end: it may start a new sequence.
                    self.reset();
                    self.scan = Scan::Agent(1);
                    return false;
                }
            }
            Scan::Osc7Body => match b {
                0x07 => {
                    out.push(b);
                    self.finish_osc7();
                }
                0x1b => {
                    self.held.push(b);
                    self.esc = true;
                }
                _ => {
                    out.push(b);
                    if self.body.len() >= OSC7_LIMIT {
                        self.reset();
                    } else {
                        self.body.push(b);
                    }
                }
            },
        }
        true
    }

    fn reset(&mut self) {
        self.scan = Scan::Text;
        self.esc = false;
        self.body.clear();
    }

    /// Keeps a known state; `none` clears it, as the agent has exited. The
    /// sequence goes in all cases.
    fn finish_agent(&mut self) {
        let value = &self.held[AGENT_OSC.len()..];
        match value.strip_suffix(b"\x1b").unwrap_or(value) {
            b"active" => self.agent = Some(AgentState::Active),
            b"blocked" => self.agent = Some(AgentState::Blocked),
            b"done" => self.agent = Some(AgentState::Done),
            b"none" => self.agent = None,
            _ => {}
        }
        self.held.clear();
        self.reset();
    }

    /// Keeps the URL-decoded path of a `file://` URL; the host is ignored.
    fn finish_osc7(&mut self) {
        let path = self.body.strip_prefix(b"file://").and_then(|rest| {
            let at = rest.iter().position(|&b| b == b'/')?;
            String::from_utf8(percent_decode(&rest[at..])).ok()
        });
        if path.is_some() {
            self.cwd = path;
        }
        self.reset();
    }
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push(high << 4 | low);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// The view that gets new output.
pub(crate) struct Sink {
    /// The token of its attach.
    pub(crate) token: u64,
    pub(crate) send: PtySink,
    /// The output frames that the view has not acknowledged.
    pub(crate) in_flight: usize,
}

pub(crate) type PtySink = Box<dyn Fn(PtyEvent) + Send>;

impl Output {
    /// Keeps and sends `event`. The output goes without the agent sequences;
    /// output that may start one waits for the next frame.
    pub(crate) fn push(&mut self, event: PtyEvent) {
        let event = match event {
            PtyEvent::Output(bytes) => PtyEvent::Output(self.osc.feed(&bytes)),
            PtyEvent::Exit(code) => {
                let held = self.osc.release();
                if !held.is_empty() {
                    self.send(PtyEvent::Output(held));
                }
                self.osc.agent = None;
                self.folder = None;
                self.exit = Some(code);
                PtyEvent::Exit(code)
            }
        };
        if !matches!(&event, PtyEvent::Output(bytes) if bytes.is_empty()) {
            self.send(event);
        }
    }

    fn send(&mut self, event: PtyEvent) {
        if let PtyEvent::Output(bytes) = &event {
            self.ring.extend(bytes);
            let excess = self.ring.len().saturating_sub(RING);
            self.ring.drain(..excess);
        }
        if let Some(sink) = &mut self.sink {
            if matches!(event, PtyEvent::Output(_)) {
                sink.in_flight += 1;
            }
            (sink.send)(event);
        }
    }

    /// The view has [`WINDOW`] frames that it has not acknowledged.
    pub(crate) fn full(&self) -> bool {
        self.sink.as_ref().is_some_and(|s| s.in_flight >= WINDOW)
    }

    /// Sends the kept output to `send`, and the exit if there is one; then
    /// new output. Takes over from the earlier view. Returns the token.
    pub(crate) fn attach(&mut self, send: PtySink) -> u64 {
        self.attaches += 1;
        let token = self.attaches;
        let mut sink = Sink { token, send, in_flight: 0 };
        // One frame, so the replay takes one slot of the ack window. The ring
        // is bounded, so the frame is too.
        let kept: Vec<u8> = self.ring.iter().copied().collect();
        if !kept.is_empty() {
            sink.in_flight += 1;
            (sink.send)(PtyEvent::Output(kept));
        }
        if let Some(code) = self.exit {
            (sink.send)(PtyEvent::Exit(code));
        }
        self.sink = Some(sink);
        self.room.notify_one();
        token
    }

    /// A stale token changes nothing.
    pub(crate) fn detach(&mut self, token: u64) {
        if self.sink.as_ref().is_some_and(|s| s.token == token) {
            self.sink = None;
            self.room.notify_one();
        }
    }

    /// A stale token changes nothing.
    pub(crate) fn ack(&mut self, token: u64) {
        if let Some(sink) = self.sink.as_mut().filter(|s| s.token == token) {
            sink.in_flight = sink.in_flight.saturating_sub(1);
            self.room.notify_one();
        }
    }
}

/// The PTY events of a session, read frame by frame.
pub(crate) struct Frames {
    pub(crate) events: Receiver<PtyEvent>,
    /// The output after the end of the last frame.
    pub(crate) carry: Vec<u8>,
}

impl Frames {
    pub(crate) fn new(events: Receiver<PtyEvent>) -> Self {
        Self { events, carry: Vec::new() }
    }

    /// Reads the next frame of at most [`FRAME`] bytes: the carried output
    /// or the output of one event, and the output queued after it up to
    /// [`FRAME_WAIT`]. A small first chunk with nothing queued after it is the
    /// frame alone. The output after [`FRAME`] bytes goes to the next frame.
    /// The exit ends a frame. `None`: the events have ended.
    pub(crate) async fn next(&mut self) -> Option<(Vec<u8>, Option<i32>)> {
        let mut bytes = if self.carry.is_empty() {
            match self.events.recv().await? {
                PtyEvent::Output(bytes) => bytes,
                PtyEvent::Exit(code) => return Some((Vec::new(), Some(code))),
            }
        } else {
            std::mem::take(&mut self.carry)
        };
        let deadline = Instant::now() + FRAME_WAIT;
        let mut alone = true;
        let mut exit = None;
        while bytes.len() < FRAME {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) if alone && bytes.len() <= ECHO => break,
                Err(TryRecvError::Empty) => match tokio::time::timeout_at(deadline, self.events.recv()).await {
                    Ok(Some(event)) => event,
                    Ok(None) | Err(_) => break,
                },
            };
            alone = false;
            match event {
                PtyEvent::Output(more) => bytes.extend(more),
                PtyEvent::Exit(code) => {
                    exit = Some(code);
                    break;
                }
            }
        }
        if bytes.len() > FRAME {
            self.carry = bytes.split_off(FRAME);
        }
        Some((bytes, exit))
    }
}

/// Keeps `folder`, read at `seq`, as the folder of the live process. Gives
/// the seq of the kept folder and true if it changed, so each seq names one
/// folder. `None`: the process has exited, or `folder` was read before a
/// newer report or change and is not kept.
pub(crate) fn keep_folder(output: &Mutex<Output>, folder: &Option<TerminalFolder>, seq: u64) -> Option<(u64, bool)> {
    let mut output = lock(output);
    if output.exit.is_some() || output.folder_seq != seq {
        return None;
    }
    if output.folder == *folder {
        return Some((seq, false));
    }
    output.folder.clone_from(folder);
    output.folder_seq += 1;
    Some((output.folder_seq, true))
}

/// Moves the PTY output into the session's output frame by frame, until the
/// process exits. While the view is full, it does not read, so the process
/// blocks on its writes. The exit code is recorded before the `Exit` event
/// is delivered, so a listing done in reaction to it shows the exit. A change
/// of the agent state of a VM process or a Mac agent goes to `agents`; at the exit, the state goes. A VM
/// process that reports a new folder by OSC 7 sends its change to `folders`.
/// The pump never asks macOS: a Mac folder comes from the batch call only.
pub(crate) async fn pump<V>(
    state: Arc<Mutex<State<V>>>,
    id: String,
    output: Arc<Mutex<Output>>,
    events: Receiver<PtyEvent>,
    kind: TerminalKind,
    agents: Arc<OnceLock<AgentSink>>,
    folders: Arc<OnceLock<FolderSink>>,
) {
    let vm = !kind.on_host();
    let reports = kind.reports_agent();
    let started = Instant::now();
    let mut frames = Frames::new(events);
    let room = lock(&output).room.clone();
    loop {
        loop {
            let notified = room.notified();
            if !lock(&output).full() {
                break;
            }
            if tokio::time::timeout(ACK_LIMIT, notified).await.is_err() {
                let mut output = lock(&output);
                if output.full() {
                    output.sink = None;
                }
            }
        }
        let Some((mut bytes, exit)) = frames.next().await else {
            return;
        };
        // A resume that found no conversation: the agent starts fresh once.
        let ran = started.elapsed();
        let fresh = match exit {
            Some(_) if ran < QUICK_EXIT => {
                lock(&state).terminals.iter_mut().find(|t| t.id == id).and_then(|t| t.fresh.take())
            }
            _ => None,
        };
        if let (Some(code), Some(open)) = (exit, fresh) {
            let reported = {
                let mut output = lock(&output);
                if !bytes.is_empty() {
                    output.push(PtyEvent::Output(std::mem::take(&mut bytes)));
                }
                output.osc.agent.is_some()
            };
            if starts_fresh(kind, ran, code, reported)
                && let Some(events) = open(id.clone()).await
            {
                lock(&output).push(PtyEvent::Output(FRESH_NOTICE.as_bytes().to_vec()));
                frames = Frames::new(events);
                continue;
            }
        }
        if let Some(code) = exit
            && let Some(t) = lock(&state).terminals.iter_mut().find(|t| t.id == id)
        {
            t.exit_code = Some(code);
        }
        let (before, after, moved) = {
            let mut output = lock(&output);
            let before = output.osc.agent;
            let cwd = output.osc.cwd.clone();
            if !bytes.is_empty() {
                output.push(PtyEvent::Output(bytes));
            }
            if let Some(code) = exit {
                output.push(PtyEvent::Exit(code));
            }
            let moved = vm && output.osc.cwd != cwd;
            if moved {
                output.folder_seq += 1;
            }
            (before, output.osc.agent.filter(|_| exit.is_none()), moved)
        };
        if reports
            && before != after
            && let Some(sink) = agents.get()
        {
            sink(&id, after);
        }
        if exit.is_some() {
            return;
        }
        if moved
            && let Ok((seq, folder)) = resolve_folder(&state, &id, None)
            && let Some((seq, true)) = keep_folder(&output, &folder, seq)
            && let Some(sink) = folders.get()
        {
            sink(&id, seq, folder.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AGENT_OSC, OSC7_LIMIT, Osc, Output, keep_folder};
    use crate::cage::testing::*;
    use crate::terminal::{AgentState, TerminalFolder};

    #[test]
    fn a_folder_read_before_a_newer_change_is_not_kept() {
        let at = |path: &str| Some(TerminalFolder { path: path.into(), vm: true, opens: None });
        let output = Mutex::new(Output::default());
        // Two reads at seq 0: the first change wins, the late one is stale.
        assert_eq!(keep_folder(&output, &at("/new"), 0), Some((1, true)));
        assert_eq!(keep_folder(&output, &at("/old"), 0), None);
        assert_eq!(lock(&output).folder, at("/new"));
        assert_eq!(keep_folder(&output, &at("/new"), 1), Some((1, false)), "no change");
        lock(&output).exit = Some(0);
        assert_eq!(keep_folder(&output, &at("/other"), 1), None);
    }

    #[tokio::test]
    async fn a_stale_detach_keeps_the_new_sink() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();
        let (old, _old_rx) = attach(&manager, &session);
        let (new, mut rx) = attach(&manager, &session);
        assert_ne!(old, new);
        manager.detach_session(&session, old);
        let _ = next(&mut rx).await;
        manager.write(&session, b"x".to_vec()).unwrap();
        while next(&mut rx).await != output("x") {}

        manager.detach_session(&session, new);
        manager.write(&session, b"y".to_vec()).unwrap();
        manager.send(&session, PtyInput::Kill).unwrap();
        until(|| manager.list().sessions[0].processes[0].exit_code == Some(137)).await;
        assert!(rx.try_recv().is_err());
        manager.detach_session("gone", new);
    }

    #[tokio::test]
    async fn the_listing_and_the_sink_get_the_agent_state_until_the_exit() {
        let (manager, _, id) = manager();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = seen.clone();
        manager.set_agent_sink(Box::new(move |process, state| lock(&into).push((process.to_owned(), state))));
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, agent(AgentKind::Pi, false), 24, 80).await.unwrap();
        let agent = || manager.list().sessions[0].processes[0].agent;
        manager.write(&session, b"\x1b]1337;zwinger-agent=blocked\x07".to_vec()).unwrap();
        until(|| agent() == Some(AgentState::Blocked)).await;
        // The same state again is no change.
        manager.write(&session, b"\x1b]1337;zwinger-agent=blocked\x07".to_vec()).unwrap();
        manager.write(&session, b"\x1b]1337;zwinger-agent=done\x07".to_vec()).unwrap();
        until(|| agent() == Some(AgentState::Done)).await;
        manager.send(&session, PtyInput::Kill).unwrap();
        until(|| manager.list().sessions[0].processes[0].exit_code.is_some()).await;
        assert_eq!(agent(), None);
        until(|| lock(&seen).len() == 3).await;
        assert_eq!(
            *lock(&seen),
            [(session.clone(), Some(AgentState::Blocked)), (session.clone(), Some(AgentState::Done)), (session, None)]
        );
    }

    #[tokio::test]
    async fn a_vm_shell_shows_the_state_of_its_agent_until_the_agent_exits() {
        let (manager, _, id) = manager();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = seen.clone();
        manager.set_agent_sink(Box::new(move |_, state| lock(&into).push(state)));
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let agent = || manager.list().sessions[0].processes[0].agent;
        manager.write(&session, b"\x1b]1337;zwinger-agent=active\x07".to_vec()).unwrap();
        until(|| agent() == Some(AgentState::Active)).await;
        // The wrapper clears the state when the agent exits; the shell lives on.
        manager.write(&session, b"\x1b]1337;zwinger-agent=none\x07".to_vec()).unwrap();
        until(|| lock(&seen).len() == 2).await;
        assert_eq!(*lock(&seen), [Some(AgentState::Active), None]);
        assert_eq!((agent(), manager.list().sessions[0].processes[0].exit_code), (None, None));
    }

    fn cwd(chunks: &[&[u8]]) -> Option<String> {
        let mut osc = Osc::default();
        for chunk in chunks {
            osc.feed(chunk);
        }
        osc.cwd
    }

    /// Feeds the chunks in order. Returns the output and the agent state.
    fn scan(chunks: &[&[u8]]) -> (Vec<u8>, Option<AgentState>) {
        let mut osc = Osc::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(osc.feed(chunk));
        }
        assert!(osc.release().is_empty());
        (out, osc.agent)
    }

    #[test]
    fn an_agent_sequence_sets_the_state_and_goes_from_the_output() {
        assert_eq!(scan(&[b"a\x1b]1337;zwinger-agent=active\x07b"]), (b"ab".to_vec(), Some(AgentState::Active)));
        assert_eq!(scan(&[b"\x1b]1337;zwinger-agent=blocked\x1b\\"]), (Vec::new(), Some(AgentState::Blocked)));
        assert_eq!(
            scan(&[b"\x1b]1337;zwinger-agent=active\x07x\x1b]1337;zwinger-agent=done\x1b\\y"]),
            (b"xy".to_vec(), Some(AgentState::Done))
        );
        assert_eq!(scan(&[b"\x1b]1337;zwinger-agent=blocked\x07\x1b]1337;zwinger-agent=none\x07"]), (Vec::new(), None));
        // An unknown value changes nothing, and goes too.
        assert_eq!(
            scan(&[b"\x1b]1337;zwinger-agent=done\x07\x1b]1337;zwinger-agent=sleepy\x07z"]),
            (b"z".to_vec(), Some(AgentState::Done))
        );
    }

    #[test]
    fn an_agent_sequence_spans_frames() {
        let whole: &[u8] = b"out\x1b]1337;zwinger-agent=blocked\x1b\\more";
        for at in 0..whole.len() {
            assert_eq!(
                scan(&[&whole[..at], &whole[at..]]),
                (b"outmore".to_vec(), Some(AgentState::Blocked)),
                "split at {at}"
            );
        }
        let bytes: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(scan(&bytes), (b"outmore".to_vec(), Some(AgentState::Blocked)));
    }

    #[test]
    fn osc7_updates_only_the_folder_and_goes_out_unchanged() {
        let osc7: &[u8] = b"\x1b]7;file://vm/workspace\x07";
        let mut osc = Osc::default();
        assert_eq!(osc.feed(b"\x1b]1337;zwinger-agent=done\x07"), b"");
        assert_eq!(osc.agent, Some(AgentState::Done));
        for chunk in osc7.chunks(3) {
            osc.feed(chunk);
        }
        assert_eq!((osc.agent, osc.cwd.as_deref()), (Some(AgentState::Done), Some("/workspace")));
        let other: &[u8] = b"\x1b[31mred\x1b]0;title\x07\x1b]1337;File=x\x07\x1b]7;file://vm/a\x1b\\$ ";
        assert_eq!(scan(&[other]), (other.to_vec(), None));
        for at in 0..other.len() {
            assert_eq!(scan(&[&other[..at], &other[at..]]).0, other, "split at {at}");
        }
    }

    #[test]
    fn a_false_prefix_is_held_until_the_next_frame_only() {
        let mut osc = Osc::default();
        assert_eq!(osc.feed(b"a\x1b]1337;zwinger"), b"a");
        assert_eq!(osc.feed(b"-x"), b"\x1b]1337;zwinger-x");
        assert_eq!(osc.feed(b"b\x1b"), b"b");
        assert_eq!(osc.feed(b"[0m"), b"\x1b[0m");
        // A value past its limit is no agent sequence.
        let long = [AGENT_OSC, &[b'a'; 40], b"\x07"].concat();
        assert_eq!(osc.feed(&long), long);
        assert_eq!(osc.agent, None);
        // At the exit, the held bytes go out.
        assert_eq!(osc.feed(b"\x1b]13"), b"");
        assert_eq!(osc.release(), b"\x1b]13");
    }

    #[test]
    fn the_ring_and_the_sink_get_the_output_without_the_agent_sequence() {
        let mut kept = Output::default();
        let sent = Arc::new(Mutex::new(Vec::<PtyEvent>::new()));
        let into = sent.clone();
        kept.attach(Box::new(move |event| lock(&into).push(event)));
        kept.push(PtyEvent::Output(b"a\x1b]1337;zwinger-agent=act".to_vec()));
        kept.push(PtyEvent::Output(b"ive\x07b\x1b]".to_vec()));
        kept.push(PtyEvent::Exit(0));
        assert!(kept.ring.iter().eq(b"ab\x1b]"));
        assert_eq!(kept.osc.agent, None);
        assert_eq!(*lock(&sent), [output("a"), output("b"), output("\x1b]"), PtyEvent::Exit(0)]);
        assert_eq!(kept.sink.as_ref().unwrap().in_flight, 3);
    }

    #[test]
    fn osc7_reports_the_last_decoded_folder() {
        assert_eq!(cwd(&[b"ls\r\n\x1b]7;file://vm/workspace/a%20b\x07$ "]).as_deref(), Some("/workspace/a b"));
        assert_eq!(cwd(&[b"\x1b]7;file://vm/one\x1b\\\x1b]7;file:///two\x07"]).as_deref(), Some("/two"));
        assert_eq!(cwd(&[b"\x1b]7;file://vm/%C3%A4\x07"]).as_deref(), Some("/\u{e4}"));
        // Other sequences and bad bodies change nothing.
        assert_eq!(cwd(&[b"\x1b]0;title\x07\x1b[31m\x1b]7;http://x/y\x07\x1b]7;file://host\x07"]), None);
        assert_eq!(cwd(&[b"\x1b]7;file://vm/%FF\x07"]), None);
        assert_eq!(cwd(&[b"\x1b]7;file://vm/a\x1b]7;file://vm/b\x07"]).as_deref(), Some("/b"));
        assert_eq!(cwd(&[b"\x1b\x1b]7;file://vm/c\x07"]).as_deref(), Some("/c"));
    }

    #[test]
    fn osc7_spans_frames() {
        let whole: &[u8] = b"out\x1b]7;file://vm/workspace/src\x1b\\more";
        for at in 0..whole.len() {
            assert_eq!(cwd(&[&whole[..at], &whole[at..]]).as_deref(), Some("/workspace/src"), "split at {at}");
        }
        let bytes: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(cwd(&bytes).as_deref(), Some("/workspace/src"));
    }

    #[test]
    fn osc7_drops_a_body_past_its_limit() {
        let mut long = b"\x1b]7;file://vm/".to_vec();
        long.extend(vec![b'a'; OSC7_LIMIT]);
        long.extend(b"\x07");
        assert_eq!(cwd(&[&long]), None);
        let mut osc = Osc::default();
        osc.feed(&long[..OSC7_LIMIT]);
        assert!(osc.body.len() <= OSC7_LIMIT);
        osc.feed(b"\x1b]7;file://vm/ok\x07");
        assert_eq!(osc.cwd.as_deref(), Some("/ok"));
    }

    #[test]
    fn the_ring_keeps_the_last_bytes_only() {
        let mut output = Output::default();
        output.push(PtyEvent::Output(vec![b'a'; RING - 1]));
        output.push(PtyEvent::Output(b"bcd".to_vec()));
        assert_eq!(output.ring.len(), RING);
        assert!(output.ring.iter().rev().take(3).eq(b"dcb"));
        assert_eq!(output.ring[0], b'a');
        output.push(PtyEvent::Output(vec![b'z'; 2 * RING]));
        assert_eq!(output.ring.len(), RING);
        assert!(output.ring.iter().all(|b| *b == b'z'));
    }

    #[tokio::test]
    async fn a_reattach_replays_the_output_in_order_and_a_stale_token_gets_nothing() {
        let (manager, _, id) = manager();
        manager.start(&id).await.unwrap();
        let session = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        for text in ["one ", "two ", "three"] {
            manager.write(&session, text.as_bytes().to_vec()).unwrap();
        }
        until(|| lock(&manager.output(&session).unwrap()).ring.len() == "24x80one two three".len()).await;

        let (old, mut old_rx) = attach(&manager, &session);
        assert_eq!(next(&mut old_rx).await, output("24x80one two three"));
        let (new, mut rx) = attach(&manager, &session);
        assert_eq!(next(&mut rx).await, output("24x80one two three"));
        manager.write(&session, b"four".to_vec()).unwrap();
        assert_eq!(next(&mut rx).await, output("four"));
        // The stale view gets no new output, and its detach and ack change nothing.
        manager.detach_session(&session, old);
        manager.ack_session(&session, old);
        assert_eq!(
            lock(&manager.output(&session).unwrap()).sink.as_ref().map(|s| (s.token, s.in_flight)),
            Some((new, 2))
        );
        manager.send(&session, PtyInput::Kill).unwrap();
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
        assert!(old_rx.try_recv().is_err());

        // After the exit, an attach gets the output and then the exit.
        let (_, mut rx) = attach(&manager, &session);
        assert_eq!(next(&mut rx).await, output("24x80one two threefour"));
        assert_eq!(next(&mut rx).await, PtyEvent::Exit(137));
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_coalesces_up_to_its_size_and_its_wait() {
        let (tx, events) = mpsc::channel(PTY_QUEUE);
        let mut frames = Frames::new(events);
        let start = Instant::now();

        // A small chunk with nothing queued goes at once.
        tx.send(output("k")).await.unwrap();
        assert_eq!(frames.next().await, Some((b"k".to_vec(), None)));
        assert_eq!(start.elapsed(), Duration::ZERO);

        // Queued chunks merge up to the frame size.
        for _ in 0..6 {
            tx.send(PtyEvent::Output(vec![b'x'; 16 << 10])).await.unwrap();
        }
        assert_eq!(frames.next().await.unwrap().0.len(), FRAME);
        assert_eq!(start.elapsed(), Duration::ZERO);

        // The rest waits for more until the frame wait ends.
        assert_eq!(frames.next().await.unwrap().0.len(), 32 << 10);
        assert_eq!(start.elapsed(), FRAME_WAIT);

        // Output that comes during the wait joins the frame; the exit ends it.
        let sender = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3)).await;
            sender.send(output("b")).await.unwrap();
            sender.send(PtyEvent::Exit(0)).await.unwrap();
        });
        tx.send(PtyEvent::Output(vec![b'a'; ECHO + 1])).await.unwrap();
        let (bytes, exit) = frames.next().await.unwrap();
        assert_eq!((bytes.len(), bytes.last(), exit), (ECHO + 2, Some(&b'b'), Some(0)));
        drop(tx);
        assert_eq!(frames.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn output_past_the_frame_size_goes_to_the_next_frame() {
        let (tx, events) = mpsc::channel(PTY_QUEUE);
        let mut frames = Frames::new(events);
        tx.send(PtyEvent::Output(vec![b'a'; 48 << 10])).await.unwrap();
        tx.send(PtyEvent::Output(vec![b'b'; 48 << 10])).await.unwrap();
        tx.send(PtyEvent::Output(vec![b'c'; 3 * FRAME])).await.unwrap();
        tx.send(PtyEvent::Exit(0)).await.unwrap();
        drop(tx);
        let mut all = Vec::new();
        let exit = loop {
            let (bytes, exit) = frames.next().await.unwrap();
            assert!(bytes.len() <= FRAME);
            all.extend(bytes);
            if exit.is_some() {
                break exit;
            }
        };
        assert_eq!(exit, Some(0));
        assert_eq!(all.len(), (96 << 10) + 3 * FRAME);
        assert!(all[..48 << 10].iter().all(|b| *b == b'a'));
        assert!(all[48 << 10..96 << 10].iter().all(|b| *b == b'b'));
        assert!(all[96 << 10..].iter().all(|b| *b == b'c'));
        assert_eq!(frames.next().await, None);
    }

    #[test]
    fn an_attach_replays_the_ring_in_one_frame() {
        let mut output = Output::default();
        output.push(PtyEvent::Output((0..RING).map(|i| i as u8).collect()));
        let sent = Arc::new(Mutex::new(Vec::<PtyEvent>::new()));
        let into = sent.clone();
        output.attach(Box::new(move |event| lock(&into).push(event)));
        let sent = lock(&sent);
        assert_eq!(sent.len(), 1);
        assert_eq!(output.sink.as_ref().unwrap().in_flight, 1);
        let PtyEvent::Output(bytes) = &sent[0] else { panic!("{:?}", sent[0]) };
        assert!(bytes.iter().eq(output.ring.iter()));
    }

    #[tokio::test]
    async fn a_burst_stays_bounded_and_a_full_view_holds_the_process() {
        let (manager, runtime, id) = manager();
        manager.start(&id).await.unwrap();
        let burst = 50 << 20;
        *runtime.burst.lock().unwrap() = burst;
        let sent = runtime.burst_sent.clone();
        let session = manager.open_session(&id, TerminalKind::Shell, 24, 80).await.unwrap();
        let (token, mut rx) = attach(&manager, &session);

        // A view that acknowledges nothing holds the process after its window.
        let mut frames = 0;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            frames += 1;
        }
        assert!(frames <= WINDOW + 1, "{frames} frames");
        // The fake prints chunks of one frame each: the window, the queue and the frame that waits.
        let held = sent.load(std::sync::atomic::Ordering::SeqCst);
        assert!(held <= (WINDOW + PTY_QUEUE + 2) * FRAME, "{held} bytes read");

        // Each acknowledged frame lets one more frame through.
        manager.ack_session(&session, token);
        assert!(next(&mut rx).await != PtyEvent::Exit(0));

        // A detached session drains into the ring.
        manager.detach_session(&session, token);
        let output = manager.output(&session).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while sent.load(std::sync::atomic::Ordering::SeqCst) < burst || lock(&output).ring.back() != Some(&b'x') {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the burst did not drain");
        assert_eq!(lock(&output).ring.len(), RING);
    }
}
