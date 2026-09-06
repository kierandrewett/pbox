//! Agent-owned PTYs. Attachments may disappear without ending the process.
use super::*;
use pbox_proto::agent::TerminalSession;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::sync::watch;

#[derive(Clone, Default)]
pub(super) struct Sessions(Arc<Mutex<BTreeMap<String, Arc<Session>>>>);

type SessionWorker = (
    mpsc::Receiver<Result<ExecRequest, Status>>,
    OwnedSemaphorePermit,
);

struct Session {
    input: mpsc::Sender<Result<ExecRequest, Status>>,
    pid: Arc<std::sync::atomic::AtomicU32>,
    state: Mutex<State>,
    close: Notify,
    finished: watch::Sender<bool>,
    attachment: watch::Sender<u64>,
}
struct State {
    info: TerminalSession,
    screen: vt100::Parser<TerminalModes>,
    generation: u64,
    output: Option<mpsc::Sender<Result<ExecEvent, Status>>>,
}

pub(super) fn check_protocol(version: u32) -> Result<(), Status> {
    if version != PROTOCOL {
        return Err(Status::failed_precondition(
            "unsupported agent protocol version",
        ));
    }
    Ok(())
}
fn dimensions(rows: u32, cols: u32) -> Result<(u16, u16), Status> {
    let rows = if rows == 0 { 24 } else { rows };
    let cols = if cols == 0 { 80 } else { cols };
    if rows > 200 || cols > 500 {
        return Err(Status::invalid_argument(
            "terminal size exceeds 200 rows or 500 columns",
        ));
    }
    Ok((rows as u16, cols as u16))
}
fn validate_name(name: &str) -> Result<(), Status> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    {
        return Err(Status::invalid_argument(
            "session names use 1–64 letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}

impl Sessions {
    pub(super) fn list(&self) -> Vec<TerminalSession> {
        self.0
            .lock()
            .unwrap()
            .values()
            .map(|session| {
                let state = session.state.lock().unwrap();
                let mut info = state.info.clone();
                info.attached = state
                    .output
                    .as_ref()
                    .is_some_and(|output| !output.is_closed());
                describe_processes(
                    &mut info,
                    session.pid.load(std::sync::atomic::Ordering::Relaxed),
                );
                info
            })
            .collect()
    }

    pub(super) async fn close(&self, name: &str) -> Result<(), Status> {
        let session = self.get(name)?;
        let mut finished = session.finished.subscribe();
        session.close.notify_one();
        finished
            .wait_for(|done| *done)
            .await
            .map_err(|_| Status::internal("session owner stopped"))?;
        Ok(())
    }

    fn prepare(
        &self,
        first: &mut ExecRequest,
        slots: &Arc<Semaphore>,
        create_only: bool,
    ) -> Result<(Arc<Session>, Option<SessionWorker>), Status> {
        check_protocol(first.protocol_version)?;
        if first.user.is_empty() {
            first.user = "pbox".into();
        }
        if !first.allocate_pty {
            return Err(Status::invalid_argument("terminal requires a PTY"));
        }
        if first.session_name.is_empty() {
            first.session_name = "main".into();
        }
        validate_name(&first.session_name)?;
        let (rows, cols) = dimensions(first.terminal_rows, first.terminal_cols)?;
        first.terminal_rows = rows.into();
        first.terminal_cols = cols.into();
        let mut registry = self.0.lock().unwrap();
        let mut start = None;
        let session = if let Some(session) = registry.get(&first.session_name) {
            if create_only {
                return Err(Status::already_exists(
                    "session already exists; read it or attach with pbox ssh",
                ));
            }
            if session.state.lock().unwrap().info.user != first.user {
                return Err(Status::failed_precondition(
                    "session belongs to a different guest user; choose another session name or use --user",
                ));
            }
            session.clone()
        } else {
            let permit = slots.clone().try_acquire_owned().map_err(|_| {
                Status::resource_exhausted(
                    "too many running commands or terminal sessions; close an unused session",
                )
            })?;
            let (input, receiver) = mpsc::channel(32);
            let session = Arc::new(Session {
                input,
                pid: Arc::default(),
                state: Mutex::new(State {
                    info: TerminalSession {
                        name: first.session_name.clone(),
                        user: first.user.clone(),
                        cwd: first.cwd.clone(),
                        argv: first.argv.clone(),
                        attached: false,
                        created_unix: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        rows: rows.into(),
                        cols: cols.into(),
                        ..Default::default()
                    },
                    screen: vt100::Parser::new_with_callbacks(
                        rows,
                        cols,
                        pbox_agent_client::HISTORY_LINES,
                        TerminalModes::default(),
                    ),
                    generation: 0,
                    output: None,
                }),
                close: Notify::new(),
                finished: watch::channel(false).0,
                attachment: watch::channel(0).0,
            });
            registry.insert(first.session_name.clone(), session.clone());
            start = Some((receiver, permit));
            session
        };
        Ok((session, start))
    }

    pub(super) async fn start(
        &self,
        mut first: ExecRequest,
        slots: &Arc<Semaphore>,
    ) -> Result<TerminalSession, Status> {
        let (session, worker) = self.prepare(&mut first, slots, true)?;
        let info = session.state.lock().unwrap().info.clone();
        let (input, permit) = worker.expect("new terminal has a worker");
        let (started, ready) = tokio::sync::oneshot::channel();
        tokio::spawn(own_session(
            self.clone(),
            session,
            first,
            input,
            permit,
            Some(started),
        ));
        ready
            .await
            .map_err(|_| Status::internal("terminal owner stopped during startup"))??;
        Ok(info)
    }

    fn get(&self, name: &str) -> Result<Arc<Session>, Status> {
        self.0
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("session '{name}' was not found")))
    }

    pub(super) fn read(
        &self,
        name: &str,
        include_history: bool,
    ) -> Result<pbox_proto::agent::ReadSessionResponse, Status> {
        let session = self.get(name)?;
        let state = session.state.lock().unwrap();
        let mut info = state.info.clone();
        info.attached = state
            .output
            .as_ref()
            .is_some_and(|output| !output.is_closed());
        let (row, col) = state.screen.screen().cursor_position();
        Ok(pbox_proto::agent::ReadSessionResponse {
            session: Some(info),
            text: state.screen.screen().contents(),
            cursor_row: row.into(),
            cursor_col: col.into(),
            history: if include_history {
                pbox_agent_client::terminal_history(state.screen.screen())
            } else {
                Vec::new()
            },
            history_supported: true,
        })
    }

    pub(super) async fn send(&self, name: &str, text: &str, keys: &[String]) -> Result<(), Status> {
        if text.len() > 64 * 1024 || keys.len() > 256 {
            return Err(Status::invalid_argument(
                "send accepts at most 64 KiB of text and 256 keys",
            ));
        }
        let session = self.get(name)?;
        let permit = tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, session.input.reserve())
            .await
            .map_err(|_| Status::deadline_exceeded("terminal input is busy"))?
            .map_err(|_| Status::not_found("terminal has ended"))?;
        let state = session.state.lock().unwrap();
        let bytes = terminal_input(&state.screen, text, keys)?;
        permit.send(Ok(ExecRequest {
            protocol_version: PROTOCOL,
            stdin: bytes,
            ..Default::default()
        }));
        Ok(())
    }

    pub(super) fn attach(
        &self,
        mut first: ExecRequest,
        requests: Streaming<ExecRequest>,
        slots: &Arc<Semaphore>,
    ) -> Result<ReceiverStream<Result<ExecEvent, Status>>, Status> {
        let (session, start) = self.prepare(&mut first, slots, false)?;
        let created = start.is_some();
        let (rows, cols) = dimensions(first.terminal_rows, first.terminal_cols)?;
        let (output, receiver) = mpsc::channel(64);
        let generation = {
            let mut state = session.state.lock().unwrap();
            state.generation += 1;
            if let Some(previous) = state.output.take() {
                let _ = previous.try_send(Err(Status::cancelled(
                    "session moved to another connection",
                )));
            }
            // Snapshot and subscription are atomic with output processing, so no bytes
            // can be lost or replayed twice between the redraw and live delivery.
            state.screen.screen_mut().set_size(rows, cols);
            if !created {
                let event = exec_event::Event::Stdout(redraw(&state.screen));
                let _ = output.try_send(Ok(ExecEvent { event: Some(event) }));
            }
            state.output = Some(output.clone());
            state.info.rows = rows.into();
            state.info.cols = cols.into();
            session.attachment.send_replace(state.generation);
            state.generation
        };
        // Resize only the existing PTY; argv, cwd and environment remain its own.
        let _ = session.input.try_send(Ok(ExecRequest {
            protocol_version: PROTOCOL,
            terminal_rows: rows.into(),
            terminal_cols: cols.into(),
            ..Default::default()
        }));
        tokio::spawn(attachment(session.clone(), generation, requests, output));
        // Subscribe before starting the process so even immediate startup output
        // and terminal queries are delivered to the initial viewer.
        if let Some((input, permit)) = start {
            tokio::spawn(own_session(
                self.clone(),
                session,
                first,
                input,
                permit,
                None,
            ));
        }
        Ok(ReceiverStream::new(receiver))
    }
}

async fn attachment(
    session: Arc<Session>,
    generation: u64,
    mut requests: Streaming<ExecRequest>,
    output: mpsc::Sender<Result<ExecEvent, Status>>,
) {
    let mut attachment = session.attachment.subscribe();
    loop {
        if *attachment.borrow_and_update() != generation {
            break;
        }
        let request = tokio::select! {
            _ = attachment.changed() => continue,
            _ = output.closed() => break,
            request = tokio::time::timeout(EXEC_STREAM_IDLE_TIMEOUT, requests.message()) => request,
        };
        let Ok(Ok(Some(request))) = request else {
            break;
        };
        if check_protocol(request.protocol_version).is_err() {
            break;
        }
        if request.stdin_eof {
            break;
        } // Detach; do not send VEOF to the shell.
        let resize = if request.terminal_rows > 0 && request.terminal_cols > 0 {
            match dimensions(request.terminal_rows, request.terminal_cols) {
                Ok(size) => Some(size),
                Err(error) => {
                    let _ = output.try_send(Err(error));
                    break;
                }
            }
        } else {
            None
        };
        // Reserve capacity before locking to keep the generation check and enqueue
        // atomic with takeover, without blocking other sessions on a full PTY.
        let permit = tokio::select! {
            _ = output.closed() => break,
            permit = tokio::time::timeout(EXEC_STDIN_WRITE_TIMEOUT, session.input.reserve()) => permit,
        };
        let Ok(Ok(permit)) = permit else {
            break;
        };
        let mut state = session.state.lock().unwrap();
        if state.generation != generation {
            break;
        }
        if let Some((rows, cols)) = resize {
            state.screen.screen_mut().set_size(rows, cols);
            state.info.rows = rows.into();
            state.info.cols = cols.into();
        }
        permit.send(Ok(request));
    }
    let mut state = session.state.lock().unwrap();
    if state.generation == generation {
        state.output.take();
        let _ = output.try_send(Ok(ExecEvent {
            event: Some(exec_event::Event::Exit(ExecExit { code: 0, signal: 0 })),
        }));
    }
}

async fn own_session(
    registry: Sessions,
    session: Arc<Session>,
    first: ExecRequest,
    input: mpsc::Receiver<Result<ExecRequest, Status>>,
    _permit: OwnedSemaphorePermit,
    started: Option<tokio::sync::oneshot::Sender<Result<(), Status>>>,
) {
    let name = first.session_name.clone();
    let (sender, mut output) = mpsc::channel(16);
    let worker = tokio::spawn(run_pty_command(
        first,
        ReceiverStream::new(input),
        sender,
        started,
        Some(session.pid.clone()),
    ));
    // The PTY input timeout is a transport safeguard. Its owner stays alive while
    // detached, without waking for every attachment or depending on a client ping.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = session.close.notified() => break,
            _ = heartbeat.tick() => {
                let _ = session.input.try_send(Ok(ExecRequest { protocol_version: PROTOCOL, ..Default::default() }));
            }
            event = output.recv() => {
                let Some(event) = event else { break; };
                let mut state = session.state.lock().unwrap();
                if let Ok(ExecEvent { event: Some(exec_event::Event::Stdout(ref bytes)) }) = event {
                    let headless = state.output.as_ref().is_none_or(|output| output.is_closed());
                    state.screen.callbacks_mut().headless = headless;
                    state.screen.process(bytes);
                    let replies = std::mem::take(&mut state.screen.callbacks_mut().replies);
                    if !replies.is_empty() {
                        let _ = session.input.try_send(Ok(ExecRequest {
                            protocol_version: PROTOCOL, stdin: replies, ..Default::default()
                        }));
                    }
                }
                if let Some(sender) = &state.output
                    && sender.try_send(event).is_err() {
                    // A slow/disconnected viewer must not block its shell. A later
                    // attach redraws the latest screen from bounded parser state.
                    state.output.take();
                    state.generation += 1;
                    session.attachment.send_replace(state.generation);
                }
            }
        }
    }
    drop(output); // Closing the worker's output cancels and reaps the process group.
    let _ = worker.await;
    {
        let mut state = session.state.lock().unwrap();
        if let Some(output) = state.output.take() {
            let _ = output.try_send(Err(Status::cancelled("terminal session closed")));
        }
        state.generation += 1;
        session.attachment.send_replace(state.generation);
    }
    registry.0.lock().unwrap().remove(&name);
    session.finished.send_replace(true);
}

// Reconstruct keyboard modes separately from screen contents. Replaying raw
// history would also replay terminal queries and clipboard operations.
#[derive(Default)]
struct TerminalModes {
    headless: bool,
    replies: Vec<u8>,
    kitty: u16,
    stack: Vec<u16>,
    modify_keys: u16,
    focus: bool,
    cursor_style: Option<u16>,
    cursor_blink: Option<bool>,
}
impl vt100::Callbacks for TerminalModes {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if pbox_agent_client::terminal_extension(screen, i1, params, command) {
            return;
        }
        let value = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let second = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
        if self.headless {
            let reply = match (i1, command, value) {
                (None, 'n', 5) => Some("\x1b[0n".to_owned()),
                (None, 'n', 6) => {
                    let (row, col) = screen.cursor_position();
                    Some(format!("\x1b[{};{}R", row + 1, col + 1))
                }
                (None, 'c', 0) => Some("\x1b[?1;2c".to_owned()),
                (Some(b'>'), 'c', 0) => Some("\x1b[>0;1;0c".to_owned()),
                (Some(b'?'), 'u', _) => Some(format!("\x1b[?{}u", self.kitty)),
                _ => None,
            };
            if let Some(reply) = reply {
                self.replies.extend(reply.bytes());
            }
        }
        match (i1, command) {
            (Some(b' '), 'q') if value <= 6 => self.cursor_style = Some(value),
            (Some(b'?'), 'h') if value == 12 => self.cursor_blink = Some(true),
            (Some(b'?'), 'l') if value == 12 => self.cursor_blink = Some(false),
            (Some(b'>'), 'u') => {
                if self.stack.len() == 16 {
                    self.stack.remove(0);
                }
                self.stack.push(self.kitty);
                self.kitty = value;
            }
            (Some(b'<'), 'u') => {
                for _ in 0..value.clamp(1, 16) {
                    self.kitty = self.stack.pop().unwrap_or(0);
                }
            }
            (Some(b'='), 'u') => {
                self.kitty = match second {
                    2 => self.kitty | value,
                    3 => self.kitty & !value,
                    _ => value,
                }
            }
            (Some(b'>'), 'm') if value == 4 => self.modify_keys = second,
            (Some(b'?'), 'h') if value == 1004 => self.focus = true,
            (Some(b'?'), 'l') if value == 1004 => self.focus = false,
            _ => {}
        }
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if self.headless && params.get(1) == Some(&&b"?"[..]) {
            match params.first().copied() {
                Some(b"10") => self
                    .replies
                    .extend_from_slice(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
                Some(b"11") => self
                    .replies
                    .extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
                _ => {}
            }
        }
    }
}
fn terminal_input(
    parser: &vt100::Parser<TerminalModes>,
    text: &str,
    keys: &[String],
) -> Result<Vec<u8>, Status> {
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
    {
        return Err(Status::invalid_argument(
            "text contains control characters; use named keys",
        ));
    }
    let mut bytes = Vec::new();
    if !text.is_empty() {
        if parser.screen().bracketed_paste() {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else if parser.callbacks().kitty & 8 != 0 {
            for c in text.chars() {
                bytes.extend(encode_key(parser, &c.to_string())?);
            }
        } else {
            bytes.extend_from_slice(text.as_bytes());
        }
    }
    for key in keys {
        bytes.extend(encode_key(parser, key)?);
    }
    Ok(bytes)
}

fn encode_key(parser: &vt100::Parser<TerminalModes>, key: &str) -> Result<Vec<u8>, Status> {
    let invalid = || {
        Status::invalid_argument(format!(
            "unknown key '{key}'; use Enter, Escape, Tab, arrows or Ctrl+C"
        ))
    };
    let mut parts: Vec<_> = key.split('+').collect();
    let name = parts.pop().ok_or_else(invalid)?;
    let mut modifiers = 0;
    for part in parts {
        modifiers |= match part.to_ascii_lowercase().as_str() {
            "shift" => 1,
            "alt" => 2,
            "ctrl" | "control" => 4,
            _ => return Err(invalid()),
        };
    }
    let lower = name.to_ascii_lowercase();
    let (code, legacy, csi) = match lower.as_str() {
        "enter" | "return" | "\n" | "\r" => (13, "\r".to_owned(), None),
        "tab" | "\t" => (9, "\t".to_owned(), None),
        "escape" | "esc" => (27, "\x1b".to_owned(), None),
        "backspace" => (127, "\x7f".to_owned(), None),
        "space" => (32, " ".to_owned(), None),
        "up" => (57352, String::new(), Some((1, 'A'))),
        "down" => (57353, String::new(), Some((1, 'B'))),
        "right" => (57351, String::new(), Some((1, 'C'))),
        "left" => (57350, String::new(), Some((1, 'D'))),
        "home" => (57356, String::new(), Some((1, 'H'))),
        "end" => (57357, String::new(), Some((1, 'F'))),
        "insert" => (57348, String::new(), Some((2, '~'))),
        "delete" => (57349, String::new(), Some((3, '~'))),
        "f1" => (57364, String::new(), Some((1, 'P'))),
        "f2" => (57365, String::new(), Some((1, 'Q'))),
        "f3" => (57366, String::new(), Some((13, '~'))),
        "f4" => (57367, String::new(), Some((1, 'S'))),
        "f5" => (57368, String::new(), Some((15, '~'))),
        "f6" => (57369, String::new(), Some((17, '~'))),
        "f7" => (57370, String::new(), Some((18, '~'))),
        "f8" => (57371, String::new(), Some((19, '~'))),
        "f9" => (57372, String::new(), Some((20, '~'))),
        "f10" => (57373, String::new(), Some((21, '~'))),
        "f11" => (57374, String::new(), Some((23, '~'))),
        "f12" => (57375, String::new(), Some((24, '~'))),
        "pageup" => (57354, String::new(), Some((5, '~'))),
        "pagedown" => (57355, String::new(), Some((6, '~'))),
        _ => {
            let mut chars = name.chars();
            let c = chars.next().ok_or_else(invalid)?;
            if chars.next().is_some() {
                return Err(invalid());
            }
            let c = if modifiers & 4 != 0 {
                c.to_ascii_lowercase()
            } else {
                c
            };
            (u32::from(c), c.to_string(), None)
        }
    };
    let flags = parser.callbacks().kitty;
    // Functional cursor keys retain their CSI encodings in the kitty protocol.
    if let Some((number, suffix)) = csi {
        return Ok(if modifiers != 0 {
            format!("\x1b[{number};{}{suffix}", modifiers + 1)
        } else if suffix == '~' {
            format!("\x1b[{number}~")
        } else if (parser.screen().application_cursor() || matches!(suffix, 'P' | 'Q' | 'S'))
            && flags == 0
        {
            format!("\x1bO{suffix}")
        } else {
            format!("\x1b[{suffix}")
        }
        .into_bytes());
    }
    if flags & 8 != 0 || (flags & 1 != 0 && (modifiers != 0 || code == 27)) {
        return Ok(format!("\x1b[{code};{}u", modifiers + 1).into_bytes());
    }
    if code == 9 && modifiers == 1 {
        return Ok(b"\x1b[Z".to_vec());
    }
    let mut bytes = Vec::new();
    if modifiers & 2 != 0 {
        bytes.push(0x1b);
    }
    if modifiers & 4 != 0 {
        let control = match code {
            9 | 13 | 27 => code as u8,
            127 => 8,
            32 | 64 => 0,
            97..=122 => (code - 96) as u8,
            91..=95 => (code - 64) as u8,
            63 => 127,
            _ => return Err(invalid()),
        };
        bytes.push(control);
    } else {
        bytes.extend_from_slice(
            if modifiers & 1 != 0 && name.chars().count() == 1 {
                name.to_uppercase()
            } else {
                legacy
            }
            .as_bytes(),
        );
    }
    Ok(bytes)
}

fn redraw(parser: &vt100::Parser<TerminalModes>) -> Vec<u8> {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let mut result = if screen.alternate_screen() {
        // A full-screen application owns an alternate buffer. Rebuild only that
        // buffer, never erase the user's normal terminal or its scrollback.
        let mut blank = vt100::Parser::new(rows, cols, 0);
        blank.process(b"\x1b[?1049h");
        let mut bytes = b"\x1b[?1049h".to_vec();
        bytes.extend(screen.state_diff(blank.screen()));
        bytes
    } else {
        pbox_agent_client::append_terminal_screen(screen)
    };
    result.extend(restore_modes(parser.callbacks()));
    result
}

fn restore_modes(modes: &TerminalModes) -> Vec<u8> {
    let mut result = Vec::new();
    if let Some(style) = modes.cursor_style {
        result.extend(format!("\x1b[{style} q").bytes());
    }
    if let Some(blink) = modes.cursor_blink {
        result.extend(format!("\x1b[?12{}", if blink { 'h' } else { 'l' }).bytes());
    }
    result.extend_from_slice(b"\x1b[<16u");
    if let Some(initial) = modes.stack.first() {
        result.extend(format!("\x1b[={initial}u").bytes());
        for flags in modes
            .stack
            .iter()
            .skip(1)
            .chain(std::iter::once(&modes.kitty))
        {
            result.extend(format!("\x1b[>{flags}u").bytes());
        }
    } else {
        result.extend(format!("\x1b[={}u", modes.kitty).bytes());
    }
    result.extend(
        format!(
            "\x1b[>4;{}m\x1b[?1004{}",
            modes.modify_keys,
            if modes.focus { 'h' } else { 'l' }
        )
        .bytes(),
    );
    result
}

// /proc is sampled only on inspection, never on the PTY output path. Session
// membership avoids confusing other shells owned by the same guest user.
fn describe_processes(info: &mut TerminalSession, pid: u32) {
    info.pid = pid;
    if pid == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(process_pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((name, fields)) = stat
            .split_once('(')
            .and_then(|(_, rest)| rest.rsplit_once(") "))
        else {
            continue;
        };
        let fields: Vec<_> = fields.split_whitespace().collect();
        if fields.len() < 6 || fields[3].parse::<u32>().ok() != Some(pid) {
            continue;
        }
        let foreground = fields[2] == fields[5];
        info.processes.push(pbox_proto::agent::SessionProcess {
            pid: process_pid,
            parent_pid: fields[1].parse().unwrap_or(0),
            name: name.to_owned(),
            foreground,
        });
    }
    info.processes.sort_by_key(|p| p.pid);
    // Prefer the deepest foreground descendant (e.g. the binary underneath a
    // launcher). All members remain available in JSON for pipelines and trees.
    let selected = info
        .processes
        .iter()
        .filter(|p| p.foreground)
        .max_by_key(|p| {
            let mut depth = 0;
            let mut parent = p.parent_pid;
            for _ in 0..info.processes.len() {
                let Some(ancestor) = info.processes.iter().find(|p| p.pid == parent) else {
                    break;
                };
                depth += 1;
                parent = ancestor.parent_pid;
            }
            (depth, p.pid)
        });
    if let Some(process) = selected {
        info.foreground_pid = process.pid;
        info.current_cwd = std::fs::read_link(format!("/proc/{}/cwd", process.pid))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_control_encodes_paste_keys_and_answers_terminal_queries_only_when_detached() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, TerminalModes::default());
        assert_eq!(
            terminal_input(
                &parser,
                "hello",
                &["Enter".into(), "Ctrl+C".into(), "Up".into()]
            )
            .unwrap(),
            b"hello\r\x03\x1b[A"
        );
        parser.process(b"\x1b[?2004h\x1b[>9u");
        assert_eq!(
            terminal_input(&parser, "hello\nworld", &["Enter".into(), "Ctrl+C".into()]).unwrap(),
            b"\x1b[200~hello\nworld\x1b[201~\x1b[13;1u\x1b[99;5u"
        );
        assert!(terminal_input(&parser, "bad\x1btext", &[]).is_err());
        assert!(terminal_input(&parser, "hello", &["unknown".into()]).is_err());
        for headless in [false, true] {
            parser.callbacks_mut().headless = headless;
            parser.process(b"\x1b[3;7H\x1b[6n\x1b[?u\x1b]11;?\x1b\\");
            assert_eq!(
                parser.callbacks().replies,
                if headless {
                    &b"\x1b[3;7R\x1b[?9u\x1b]11;rgb:0000/0000/0000\x1b\\"[..]
                } else {
                    &[][..]
                }
            );
        }
    }

    #[test]
    fn shell_replay_appends_without_clearing_the_local_screen() {
        let mut remote = vt100::Parser::new_with_callbacks(24, 80, 200, TerminalModes::default());
        remote.process(b"previous output\r\n$ typed");
        let replay = redraw(&remote);
        for clear in [b"\x1b[2J".as_slice(), b"\x1b[3J", b"\x1b[H", b"\x1b[?1049l"] {
            assert!(!replay.windows(clear.len()).any(|bytes| bytes == clear));
        }
        let mut local = vt100::Parser::new(24, 80, 200);
        local.process(b"LOCAL HISTORY MUST STAY");
        local.process(&replay);
        assert_eq!(
            local.screen().contents(),
            "LOCAL HISTORY MUST STAY\nprevious output\n$ typed"
        );
        assert_eq!(local.screen().cursor_position(), (2, 7));
    }

    #[test]
    fn screen_replay_restores_modes_without_replaying_queries_or_clipboards() {
        let input = b"\x1b[?1049h\x1b[2J\x1b[Heditor\x1b[?2004h\x1b[>1u\x1b[>3u\x1b[>4;2m\x1b[?1004h\x1b[6n\x1b]52;c;secret\x07";
        for chunk in 1..=input.len() {
            let mut parser =
                vt100::Parser::new_with_callbacks(24, 80, 200, TerminalModes::default());
            for bytes in input.chunks(chunk) {
                parser.process(bytes);
            }
            let bytes = redraw(&parser);
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("[6n") && !text.contains("secret"));
            let mut restored =
                vt100::Parser::new_with_callbacks(24, 80, 200, TerminalModes::default());
            restored.process(&bytes);
            assert!(restored.screen().alternate_screen());
            assert!(restored.screen().bracketed_paste());
            assert_eq!(restored.screen().contents(), "editor");
            assert_eq!(restored.callbacks().kitty, 3);
            restored.process(b"\x1b[<u");
            assert_eq!(restored.callbacks().kitty, 1);
            assert_eq!(restored.callbacks().modify_keys, 2);
            assert!(restored.callbacks().focus);
        }
    }
}
