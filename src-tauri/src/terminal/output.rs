use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::Instant;

use crate::runtime::PtyEvent;
use crate::shield::manager::{State, lock};

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
    pub(crate) fn push(&mut self, event: PtyEvent) {
        match &event {
            PtyEvent::Output(bytes) => {
                self.ring.extend(bytes);
                let excess = self.ring.len().saturating_sub(RING);
                self.ring.drain(..excess);
            }
            PtyEvent::Exit(code) => self.exit = Some(*code),
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

/// Moves the PTY output into the session's output frame by frame, until the
/// process exits. While the view is full, it does not read, so the process
/// blocks on its writes. The exit code is recorded before the `Exit` event
/// is delivered, so a listing done in reaction to it shows the exit.
pub(crate) async fn pump<V>(
    state: Arc<Mutex<State<V>>>,
    session_id: String,
    output: Arc<Mutex<Output>>,
    events: Receiver<PtyEvent>,
) {
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
        let Some((bytes, exit)) = frames.next().await else {
            return;
        };
        if let Some(code) = exit
            && let Some(s) = lock(&state).sessions.iter_mut().find(|s| s.id == session_id)
        {
            s.exit_code = Some(code);
        }
        let mut output = lock(&output);
        if !bytes.is_empty() {
            output.push(PtyEvent::Output(bytes));
        }
        if let Some(code) = exit {
            output.push(PtyEvent::Exit(code));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::shield::testing::*;

    #[tokio::test]
    async fn a_stale_detach_keeps_the_new_sink() {
        let (manager, _, id) = manager();
        let web = app(&manager, &id, "pnpm dev", false);
        manager.start(&id).await.unwrap();
        let session = manager.start_app(&id, &web).await.unwrap();
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
        until(|| runs(&manager) == [(web.clone(), Some(137))]).await;
        assert!(rx.try_recv().is_err());
        manager.detach_session("gone", new);
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
        let session = manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
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
        let session = manager.open_session(&id, SessionKind::Shell, 24, 80).await.unwrap();
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
