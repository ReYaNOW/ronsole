//! Delivery of `TAB_INPUT` requests into a tab: receipt checks, the
//! timer-driven write / Enter / release sequence, and the per-tab record of the
//! last delivered `/clear` that a following phrase request must match.

use super::App;
use crate::platform::single_instance::{
    PeerCred, TabInputCode, TabInputEnvelope, TabInputKind, TabInputRequest,
    validate_tab_input_text,
};
use crate::tab_input::{InputBox, ProcGen, claude_generation, classify_input_box, origin_allowed};
use crate::terminal::Terminal;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

const TAB_TOKEN_HEX_LEN: usize = 32;
const INPUT_BOX_ROWS: usize = 12;
const RECENT_USER_INPUT: Duration = Duration::from_millis(1000);
const ACCEPT_INTERVAL: Duration = Duration::from_millis(300);
const ENTER_DELAY: Duration = Duration::from_millis(150);
const HOLD_RELEASE_DELAY: Duration = Duration::from_millis(100);
const CLEAR_CAUSALITY: Duration = Duration::from_secs(10);

/// Process lookups behind the receipt and delivery checks; replaceable in tests.
#[derive(Clone, Copy)]
pub(super) struct TabInputProbe {
    claude_generation: fn(u32) -> Option<ProcGen>,
    origin_allowed: fn(u32, u32) -> bool,
}

impl Default for TabInputProbe {
    fn default() -> Self {
        Self {
            claude_generation,
            origin_allowed,
        }
    }
}

/// The last `/clear` delivered into a tab. `at` is when the line was written
/// (the user-input hold began), so keys typed while the hold ran also count as
/// input after the `/clear`.
struct ClearOp {
    op: [u8; 16],
    at: Instant,
    generation: ProcGen,
}

struct TabRecord {
    token: String,
    last_accept: Option<Instant>,
    clear_op: Option<ClearOp>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the request delay; then hold user input and write the line.
    Write,
    /// Line written; waiting to send `\r`.
    Enter,
    /// `\r` written and the reply sent; waiting to end the user-input hold.
    Release,
}

struct Delivery {
    token: String,
    kind: TabInputKind,
    op: [u8; 16],
    line: String,
    generation: ProcGen,
    /// For a phrase: when the matched `/clear` was written; user input newer
    /// than this refuses the phrase at the write step.
    clear_at: Option<Instant>,
    reply: Sender<TabInputCode>,
    phase: Phase,
    deadline: Instant,
    started: Instant,
}

#[derive(Default)]
pub(super) struct TabInputState {
    probe: TabInputProbe,
    records: Vec<TabRecord>,
    deliveries: Vec<Delivery>,
}

impl TabInputState {
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deliveries
            .iter()
            .map(|delivery| delivery.deadline)
            .min()
    }
}

impl App {
    pub(super) fn handle_tab_input(&mut self, envelope: TabInputEnvelope) {
        self.handle_tab_input_at(envelope, Instant::now());
    }

    fn handle_tab_input_at(&mut self, envelope: TabInputEnvelope, now: Instant) {
        let TabInputEnvelope {
            request,
            peer,
            reply,
        } = envelope;
        match self.accept_tab_input(&request, peer, now) {
            Ok((generation, clear_at)) => self.tab_input.deliveries.push(Delivery {
                token: request.token,
                kind: request.kind,
                op: request.op,
                line: request.line,
                generation,
                clear_at,
                reply,
                phase: Phase::Write,
                deadline: now + Duration::from_millis(u64::from(request.delay_ms)),
                started: now,
            }),
            Err(code) => {
                let _ = reply.send(code);
            }
        }
    }

    /// Receipt checks in the protocol order; the first failing check names the
    /// reply code. On success the tab's accept time is recorded.
    fn accept_tab_input(
        &mut self,
        request: &TabInputRequest,
        peer: PeerCred,
        now: Instant,
    ) -> Result<(ProcGen, Option<Instant>), TabInputCode> {
        let index = self
            .find_tab_by_token(&request.token)
            .ok_or(TabInputCode::NoTab)?;
        let probe = self.tab_input.probe;
        let terminal = &self.terminals[index];
        let leader = terminal
            .process_group_leader()
            .ok_or(TabInputCode::LeaderNotClaude)?;
        let generation =
            (probe.claude_generation)(leader).ok_or(TabInputCode::LeaderNotClaude)?;
        if !(probe.origin_allowed)(peer.pid, leader) {
            return Err(TabInputCode::PeerRejected);
        }
        if !validate_tab_input_text(request.kind, &request.line) {
            return Err(TabInputCode::InvalidInput);
        }
        self.tab_input.records.retain(|record| {
            self.terminals
                .iter()
                .any(|terminal| terminal.tab_token() == record.token)
        });
        let record = tab_record(&mut self.tab_input.records, &request.token);
        let clear_at = match request.kind {
            TabInputKind::Line | TabInputKind::Clear => {
                check_input_ready(terminal, now)?;
                None
            }
            // A phrase request consumes the remembered /clear whatever the outcome.
            TabInputKind::PhraseAfterClear => Some(check_clear_cause(
                record.clear_op.take(),
                request.op,
                terminal,
                generation,
                now,
            )?),
        };
        // A delivery in its release phase has already replied and only waits to
        // end the input hold, so it does not occupy the tab's queue slot.
        let queued = self
            .tab_input
            .deliveries
            .iter()
            .any(|delivery| delivery.token == request.token && delivery.phase != Phase::Release);
        let too_soon = record
            .last_accept
            .is_some_and(|at| now.saturating_duration_since(at) < ACCEPT_INTERVAL);
        if queued || too_soon {
            return Err(TabInputCode::RateLimitedOrQueued);
        }
        record.last_accept = Some(now);
        Ok((generation, clear_at))
    }

    /// A tab is addressed only by a well-formed token; an empty tab token
    /// (failed randomness) can therefore never match.
    fn find_tab_by_token(&self, token: &str) -> Option<usize> {
        let well_formed = token.len() == TAB_TOKEN_HEX_LEN
            && token
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        if !well_formed {
            return None;
        }
        self.terminals
            .iter()
            .position(|terminal| terminal.tab_token() == token)
    }

    pub(super) fn advance_due_tab_deliveries(&mut self) {
        if self.tab_input.deliveries.is_empty() {
            return;
        }
        self.advance_tab_deliveries(Instant::now());
    }

    fn advance_tab_deliveries(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.tab_input.deliveries.len() {
            if self.tab_input.deliveries[index].deadline > now {
                index += 1;
                continue;
            }
            let delivery = self.tab_input.deliveries.remove(index);
            if let Some(delivery) = self.step_delivery(delivery, now) {
                self.tab_input.deliveries.insert(index, delivery);
                index += 1;
            }
        }
    }

    /// Runs the phase whose deadline is due. Returns the delivery when it
    /// continues with a later deadline; every refusal replies once and ends the
    /// user-input hold before returning `None`.
    fn step_delivery(&mut self, mut delivery: Delivery, now: Instant) -> Option<Delivery> {
        let probe = self.tab_input.probe;
        let Some(index) = self.find_tab_by_token(&delivery.token) else {
            // The held buffer is lost together with the tab.
            if delivery.phase != Phase::Release {
                let _ = delivery.reply.send(TabInputCode::NoTab);
            }
            return None;
        };
        let terminal = &mut self.terminals[index];
        match delivery.phase {
            Phase::Write => {
                let checked = check_same_leader(probe, terminal, delivery.generation).and_then(|()| {
                    match delivery.kind {
                        TabInputKind::Line | TabInputKind::Clear => check_input_ready(terminal, now),
                        TabInputKind::PhraseAfterClear => {
                            check_no_input_since(terminal, delivery.clear_at)
                        }
                    }
                });
                if let Err(code) = checked {
                    let _ = delivery.reply.send(code);
                    return None;
                }
                terminal.begin_input_hold();
                if terminal.write_input(delivery.line.as_bytes()).is_err() {
                    terminal.end_input_hold();
                    let _ = delivery.reply.send(TabInputCode::PtyWriteFailed);
                    return None;
                }
                delivery.started = now;
                delivery.phase = Phase::Enter;
                delivery.deadline = now + ENTER_DELAY;
                Some(delivery)
            }
            Phase::Enter => {
                if let Err(code) = check_same_leader(probe, terminal, delivery.generation) {
                    terminal.end_input_hold();
                    let _ = delivery.reply.send(code);
                    return None;
                }
                if terminal.write_input(b"\r").is_err() {
                    terminal.end_input_hold();
                    let _ = delivery.reply.send(TabInputCode::PtyWriteFailed);
                    return None;
                }
                let _ = delivery.reply.send(TabInputCode::InputDelivered);
                if delivery.kind == TabInputKind::Clear {
                    tab_record(&mut self.tab_input.records, &delivery.token).clear_op =
                        Some(ClearOp {
                            op: delivery.op,
                            at: delivery.started,
                            generation: delivery.generation,
                        });
                }
                delivery.phase = Phase::Release;
                delivery.deadline = now + HOLD_RELEASE_DELAY;
                Some(delivery)
            }
            Phase::Release => {
                terminal.end_input_hold();
                None
            }
        }
    }
}

fn tab_record<'a>(records: &'a mut Vec<TabRecord>, token: &str) -> &'a mut TabRecord {
    let index = match records.iter().position(|record| record.token == token) {
        Some(index) => index,
        None => {
            records.push(TabRecord {
                token: token.to_owned(),
                last_accept: None,
                clear_op: None,
            });
            records.len() - 1
        }
    };
    &mut records[index]
}

/// The input box shows the empty busy prompt and the user has not typed recently.
fn check_input_ready(terminal: &Terminal, now: Instant) -> Result<(), TabInputCode> {
    let rows = terminal.screen_tail_text(INPUT_BOX_ROWS);
    let verdict = classify_input_box(&rows);
    if verdict != InputBox::EmptyBusy {
        #[cfg(not(test))]
        dump_input_box(&rows, verdict);
        return Err(TabInputCode::InputBoxUnavailable);
    }
    let recent = terminal
        .last_user_input()
        .is_some_and(|at| now.saturating_duration_since(at) < RECENT_USER_INPUT);
    if recent {
        return Err(TabInputCode::RecentUserInput);
    }
    Ok(())
}

/// Diagnostic: the rows the last code-6 refusal saw, in
/// `$XDG_RUNTIME_DIR/cc-self/ronsole-input-box.txt` (overwritten each time).
#[cfg(not(test))]
fn dump_input_box(rows: &[String], verdict: InputBox) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join("cc-self").join("ronsole-input-box.txt");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
    else {
        return;
    };
    let mut text = format!("===== {:?} {:?}\n", std::time::SystemTime::now(), verdict);
    for (index, row) in rows.iter().enumerate() {
        text.push_str(&format!("{index:02}|{row}|\n"));
    }
    let _ = file.write_all(text.as_bytes());
}

/// The leader must still be the same `claude` process generation.
fn check_same_leader(
    probe: TabInputProbe,
    terminal: &Terminal,
    expected: ProcGen,
) -> Result<(), TabInputCode> {
    let current = terminal
        .process_group_leader()
        .and_then(probe.claude_generation);
    if current == Some(expected) {
        Ok(())
    } else {
        Err(TabInputCode::LeaderChanged)
    }
}

/// A phrase needs our own `/clear` with the same `op`, at most 10 s old, with
/// no user input since it and the same leader generation.
fn check_clear_cause(
    clear_op: Option<ClearOp>,
    op: [u8; 16],
    terminal: &Terminal,
    generation: ProcGen,
    now: Instant,
) -> Result<Instant, TabInputCode> {
    let Some(clear) = clear_op else {
        return Err(TabInputCode::ClearCauseMissing);
    };
    check_no_input_since(terminal, Some(clear.at))?;
    if clear.op != op || now.saturating_duration_since(clear.at) > CLEAR_CAUSALITY {
        return Err(TabInputCode::ClearCauseMissing);
    }
    if clear.generation != generation {
        return Err(TabInputCode::LeaderChanged);
    }
    Ok(clear.at)
}

/// The user has not typed since the `/clear` was written, so a phrase cannot
/// end up appended to their draft.
fn check_no_input_since(terminal: &Terminal, since: Option<Instant>) -> Result<(), TabInputCode> {
    let typed_since = match (terminal.last_user_input(), since) {
        (Some(typed), Some(since)) => typed > since,
        _ => false,
    };
    if typed_since {
        Err(TabInputCode::ClearCauseMissing)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::TerminalLaunchSpec;
    use crate::app::AppLoopControl;
    use std::cell::Cell;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::SystemTime;

    const EMPTY_BOX: &str = "────────────────────\r\n❯\r\n────────────────────\r\n";
    const FILLED_BOX: &str = "────────────────────\r\n❯ draft\r\n────────────────────\r\n";
    const OP_A: [u8; 16] = [0xa1; 16];
    const OP_B: [u8; 16] = [0xb2; 16];
    const LINE: &str = "/effort high";
    const PHRASE: &str = "continue with the plan";

    thread_local! {
        static LEADER_STARTTIME: Cell<Option<u64>> = const { Cell::new(Some(100)) };
        static ORIGIN_OK: Cell<bool> = const { Cell::new(true) };
    }

    fn fake_generation(pid: u32) -> Option<ProcGen> {
        LEADER_STARTTIME
            .with(Cell::get)
            .map(|starttime| ProcGen { pid, starttime })
    }

    fn fake_origin(_sender: u32, _leader: u32) -> bool {
        ORIGIN_OK.with(Cell::get)
    }

    fn set_leader(starttime: Option<u64>) {
        LEADER_STARTTIME.with(|cell| cell.set(starttime));
    }

    struct Fixture {
        app: App,
        root: PathBuf,
        token: String,
        t0: Instant,
    }

    impl Fixture {
        /// A tab whose child prints `screen`, then copies PTY input into `out`.
        fn new(screen: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "ronsole-tab-delivery-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let script = "stty raw -echo; i=0; while [ $i -lt 80 ]; do printf '\\r\\n'; \
                          i=$((i+1)); done; printf '%s' \"$1\"; printf __READY__; \
                          exec cat > out";
            let launch = TerminalLaunchSpec {
                working_directory: Some(root.clone()),
                command: vec![
                    OsString::from("/bin/sh"),
                    OsString::from("-c"),
                    OsString::from(script),
                    OsString::from("sh"),
                    OsString::from(screen),
                ],
                hold: false,
                bridge_launch_id: None,
            };
            let terminal = Terminal::spawn(None, 1, launch);
            let token = terminal.tab_token().to_owned();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let ready = crate::platform::lock_recover(&terminal.grid)
                    .lines
                    .iter()
                    .flat_map(|line| line.iter().map(|cell| cell.c))
                    .collect::<String>()
                    .contains("__READY__");
                if ready && root.join("out").exists() {
                    break;
                }
                assert!(Instant::now() < deadline, "PTY fixture did not become ready");
                std::thread::sleep(Duration::from_millis(10));
            }
            set_leader(Some(100));
            ORIGIN_OK.with(|cell| cell.set(true));
            let mut app = App::new();
            app.tab_input.probe = TabInputProbe {
                claude_generation: fake_generation,
                origin_allowed: fake_origin,
            };
            app.terminals.push(terminal);
            Self {
                app,
                root,
                token,
                t0: Instant::now(),
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        fn send(
            &mut self,
            token: &str,
            kind: TabInputKind,
            op: [u8; 16],
            line: &str,
            now_ms: u64,
        ) -> Receiver<TabInputCode> {
            let (reply, receiver) = channel();
            let envelope = TabInputEnvelope {
                request: TabInputRequest {
                    token: token.to_owned(),
                    delay_ms: 100,
                    kind,
                    op,
                    line: line.to_owned(),
                },
                peer: PeerCred { uid: 1000, pid: 4242 },
                reply,
            };
            let now = self.at(now_ms);
            self.app.handle_tab_input_at(envelope, now);
            receiver
        }

        fn send_own(
            &mut self,
            kind: TabInputKind,
            op: [u8; 16],
            line: &str,
            now_ms: u64,
        ) -> Receiver<TabInputCode> {
            let token = self.token.clone();
            self.send(&token, kind, op, line, now_ms)
        }

        fn advance(&mut self, now_ms: u64) {
            let now = self.at(now_ms);
            self.app.advance_tab_deliveries(now);
        }

        fn set_user_input(&mut self, at_ms: Option<u64>) {
            let at = at_ms.map(|ms| self.at(ms));
            self.app.terminals[0].set_last_user_input_for_test(at);
        }

        fn out(&self) -> Vec<u8> {
            std::fs::read(self.root.join("out")).unwrap_or_default()
        }

        fn wait_out(&self, expected: &[u8]) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.out() != expected {
                assert!(
                    Instant::now() < deadline,
                    "PTY got {:?}, expected {:?}",
                    String::from_utf8_lossy(&self.out()),
                    String::from_utf8_lossy(expected)
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(40));
            assert_eq!(self.out(), expected, "extra bytes reached the PTY");
        }

        fn held(&self) -> bool {
            self.app.terminals[0].input_hold_active()
        }

        /// Runs a kind 1 delivery to the end: sent at 0, written at 100, `\r`
        /// at 250, released at 350 ms.
        fn deliver_clear(&mut self, op: [u8; 16]) {
            self.deliver_clear_until_enter(op);
            self.advance(350);
            assert!(!self.held());
            assert!(self.app.tab_input.deliveries.is_empty());
        }
    }

    impl Fixture {
        /// Runs a kind 1 delivery up to the reply `0` at 250 ms; the delivery
        /// stays in the release phase until 350 ms.
        fn deliver_clear_until_enter(&mut self, op: [u8; 16]) {
            let reply = self.send_own(TabInputKind::Clear, op, "/clear", 0);
            self.advance(100);
            self.advance(250);
            assert_eq!(reply.try_recv(), Ok(TabInputCode::InputDelivered));
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn line_is_written_then_enter_separately_then_reply_ok() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        assert_eq!(fx.app.tab_input.next_deadline(), Some(fx.at(100)));
        assert_eq!(
            fx.app.loop_control(),
            AppLoopControl::WaitUntil(fx.at(100))
        );

        fx.advance(99);
        assert_eq!(fx.app.tab_input.next_deadline(), Some(fx.at(100)));
        assert!(!fx.held());

        fx.advance(100);
        assert!(fx.held());
        fx.wait_out(LINE.as_bytes());
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        assert_eq!(fx.app.tab_input.next_deadline(), Some(fx.at(250)));

        fx.advance(250);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InputDelivered));
        fx.wait_out(b"/effort high\r");
        assert!(fx.held());
        assert_eq!(fx.app.tab_input.next_deadline(), Some(fx.at(350)));

        fx.advance(350);
        assert!(!fx.held());
        assert_eq!(fx.app.tab_input.next_deadline(), None);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn user_key_during_delivery_goes_to_pty_after_enter() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.advance(100);
        fx.app.terminals[0].write_user_input(b"k");
        fx.wait_out(LINE.as_bytes());
        fx.advance(250);
        fx.wait_out(b"/effort high\r");
        fx.advance(350);
        fx.wait_out(b"/effort high\rk");
    }

    #[test]
    fn non_empty_or_unrecognized_input_box_replies_6_at_receipt_and_at_deadline() {
        let mut fx = Fixture::new(FILLED_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InputBoxUnavailable));
        assert!(fx.app.tab_input.deliveries.is_empty());

        let mut fx = Fixture::new("plain output\r\n");
        let reply = fx.send_own(TabInputKind::Clear, OP_A, "/clear", 0);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InputBoxUnavailable));
    }

    #[test]
    fn input_box_filled_after_receipt_is_refused_at_deadline_without_hold() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        {
            let mut grid = crate::platform::lock_recover(&fx.app.terminals[0].grid);
            let row = grid.lines.len() - 3;
            grid.lines[row][2].c = 'x';
        }
        fx.advance(100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InputBoxUnavailable));
        assert!(!fx.held());
        assert!(fx.app.tab_input.deliveries.is_empty());
        fx.wait_out(b"");
    }

    #[test]
    fn recent_user_input_replies_7_until_1000_ms_old() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.set_user_input(Some(0));
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 500);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::RecentUserInput));

        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 1000);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    #[test]
    fn user_input_after_receipt_refuses_at_deadline_and_releases_held_keys() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.set_user_input(Some(80));
        fx.advance(100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::RecentUserInput));
        assert!(!fx.held());
        fx.wait_out(b"");
    }

    #[test]
    fn second_request_while_queued_or_within_300_ms_replies_5() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let first = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        let second = fx.send_own(TabInputKind::Line, [0; 16], LINE, 10);
        assert_eq!(second.try_recv(), Ok(TabInputCode::RateLimitedOrQueued));
        assert_eq!(first.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        // The first request fails at its deadline; its acceptance still counts for 300 ms.
        fx.set_user_input(Some(100));
        fx.advance(100);
        assert_eq!(first.try_recv(), Ok(TabInputCode::RecentUserInput));
        fx.set_user_input(None);
        let third = fx.send_own(TabInputKind::Line, [0; 16], LINE, 299);
        assert_eq!(third.try_recv(), Ok(TabInputCode::RateLimitedOrQueued));
        let fourth = fx.send_own(TabInputKind::Line, [0; 16], LINE, 300);
        assert_eq!(fourth.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    #[test]
    fn leader_change_before_deadline_replies_8_without_hold() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        set_leader(Some(200));
        fx.advance(100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::LeaderChanged));
        assert!(!fx.held());
        fx.wait_out(b"");
    }

    #[test]
    fn leader_change_before_enter_replies_8_ends_hold_and_flushes_keys() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.advance(100);
        fx.app.terminals[0].write_user_input(b"k");
        set_leader(None);
        fx.advance(250);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::LeaderChanged));
        assert!(!fx.held());
        assert!(fx.app.tab_input.deliveries.is_empty());
        fx.wait_out(b"/effort highk");
    }

    #[test]
    fn pty_write_failure_replies_9_and_ends_hold_at_both_writes() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.app.terminals[0].break_input_writer_for_test();
        fx.advance(100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::PtyWriteFailed));
        assert!(!fx.held());
        assert!(fx.app.tab_input.deliveries.is_empty());

        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.advance(100);
        assert!(fx.held());
        fx.app.terminals[0].break_input_writer_for_test();
        fx.advance(250);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::PtyWriteFailed));
        assert!(!fx.held());
        assert!(fx.app.tab_input.deliveries.is_empty());
    }

    #[test]
    fn tab_closed_before_or_during_delivery_replies_1() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.app.terminals.remove(0);
        fx.advance(100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::NoTab));
        assert!(fx.app.tab_input.deliveries.is_empty());

        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::Line, [0; 16], LINE, 0);
        fx.advance(100);
        assert!(fx.held());
        fx.app.terminals.remove(0);
        fx.advance(250);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::NoTab));
        assert!(fx.app.tab_input.deliveries.is_empty());
    }

    #[test]
    fn token_must_be_32_lowercase_hex_and_match_a_tab() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let upper = fx.token.to_uppercase();
        let short = fx.token[..31].to_owned();
        let long = format!("{}0", fx.token);
        let non_hex = format!("{}g", &fx.token[..31]);
        let unknown = "0".repeat(32);
        for token in ["", &upper, &short, &long, &non_hex, &unknown] {
            let reply = fx.send(token, TabInputKind::Line, [0; 16], LINE, 0);
            assert_eq!(reply.try_recv(), Ok(TabInputCode::NoTab), "token {token:?}");
        }
        assert!(fx.app.tab_input.deliveries.is_empty());
    }

    #[test]
    fn leader_origin_text_checks_run_in_order() {
        let mut fx = Fixture::new(EMPTY_BOX);
        set_leader(None);
        ORIGIN_OK.with(|cell| cell.set(false));
        let reply = fx.send_own(TabInputKind::Line, [0; 16], "bad", 0);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::LeaderNotClaude));

        set_leader(Some(100));
        let reply = fx.send_own(TabInputKind::Line, [0; 16], "bad", 0);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::PeerRejected));

        ORIGIN_OK.with(|cell| cell.set(true));
        let reply = fx.send_own(TabInputKind::Line, [0; 16], "bad", 0);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InvalidInput));
    }

    #[test]
    fn invalid_text_for_each_kind_replies_4() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let cases = [
            (TabInputKind::Line, "hello"),
            (TabInputKind::Line, "/effort hi\u{1b}gh"),
            (TabInputKind::Clear, "/clear now"),
            (TabInputKind::Clear, ""),
            (TabInputKind::PhraseAfterClear, " /effort high"),
            (TabInputKind::PhraseAfterClear, "ok\u{202e}"),
        ];
        for (kind, line) in cases {
            let reply = fx.send_own(kind, OP_A, line, 0);
            assert_eq!(reply.try_recv(), Ok(TabInputCode::InvalidInput), "{line:?}");
        }
        assert!(fx.app.tab_input.deliveries.is_empty());
    }

    #[test]
    fn phrase_after_delivered_clear_is_typed_once() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        fx.wait_out(b"/clear\r");

        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 2000);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        fx.advance(2100);
        fx.advance(2250);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InputDelivered));
        fx.advance(2350);
        fx.wait_out(b"/clear\rcontinue with the plan\r");

        let again = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 4000);
        assert_eq!(again.try_recv(), Ok(TabInputCode::ClearCauseMissing));
    }

    #[test]
    fn phrase_without_matching_clear_replies_10() {
        let mut fx = Fixture::new(EMPTY_BOX);
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 0);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::ClearCauseMissing));

        // A foreign op is refused and also consumes the remembered /clear.
        fx.deliver_clear(OP_A);
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_B, PHRASE, 1000);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::ClearCauseMissing));
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 2000);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::ClearCauseMissing));
    }

    #[test]
    fn phrase_after_11_seconds_replies_10_and_after_10_seconds_is_accepted() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 11_000);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::ClearCauseMissing));

        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 10_050);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    #[test]
    fn phrase_after_user_input_since_clear_replies_10() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        fx.set_user_input(Some(500));
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 2000);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::ClearCauseMissing));
    }

    #[test]
    fn phrase_during_clear_release_phase_is_not_queued() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear_until_enter(OP_A);
        assert_eq!(fx.app.tab_input.deliveries.len(), 1);

        // Past the 300 ms accept interval but inside the clear's 100 ms release phase.
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 320);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        assert_eq!(fx.app.tab_input.deliveries.len(), 2);

        fx.advance(350);
        assert!(!fx.held());
        fx.advance(420);
        fx.advance(570);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::InputDelivered));
        fx.advance(670);
        fx.wait_out(b"/clear\rcontinue with the plan\r");
    }

    #[test]
    fn phrase_refused_at_deadline_when_user_typed_while_waiting() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 2000);
        assert_eq!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        fx.app.terminals[0].write_user_input(b"k");
        fx.set_user_input(Some(2050));
        fx.advance(2100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::ClearCauseMissing));
        assert!(!fx.held());
        assert!(fx.app.tab_input.deliveries.is_empty());
        fx.wait_out(b"/clear\rk");

        let again = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 4000);
        assert_eq!(again.try_recv(), Ok(TabInputCode::ClearCauseMissing));
    }

    #[test]
    fn phrase_after_leader_change_replies_8() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        set_leader(Some(300));
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 2000);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::LeaderChanged));
    }

    #[test]
    fn phrase_leader_change_before_deadline_replies_8() {
        let mut fx = Fixture::new(EMPTY_BOX);
        fx.deliver_clear(OP_A);
        let reply = fx.send_own(TabInputKind::PhraseAfterClear, OP_A, PHRASE, 2000);
        set_leader(Some(300));
        fx.advance(2100);
        assert_eq!(reply.try_recv(), Ok(TabInputCode::LeaderChanged));
        assert!(!fx.held());
    }
}
