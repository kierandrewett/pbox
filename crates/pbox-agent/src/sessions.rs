//! Agent-owned PTYs. Attachments may disappear without ending the process.
use super::*;
use pbox_proto::agent::TerminalSession;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::sync::watch;

#[derive(Clone, Default)]
pub(super) struct Sessions(Arc<Mutex<BTreeMap<String, Arc<Session>>>>);

struct Session {
    input: mpsc::Sender<Result<ExecRequest, Status>>,
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
                info
            })
            .collect()
    }

    pub(super) async fn close(&self, name: &str) -> Result<(), Status> {
        let session = self
            .0
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("session '{name}' was not found")))?;
        let mut finished = session.finished.subscribe();
        session.close.notify_one();
        finished
            .wait_for(|done| *done)
            .await
            .map_err(|_| Status::internal("session owner stopped"))?;
        Ok(())
    }

    pub(super) fn attach(
        &self,
        mut first: ExecRequest,
        requests: Streaming<ExecRequest>,
        slots: &Arc<Semaphore>,
    ) -> Result<ReceiverStream<Result<ExecEvent, Status>>, Status> {
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
        let (session, created) = if let Some(session) = registry.get(&first.session_name) {
            if session.state.lock().unwrap().info.user != first.user {
                return Err(Status::failed_precondition(
                    "session belongs to a different guest user; choose another session name or use --user",
                ));
            }
            (session.clone(), false)
        } else {
            let permit = slots.clone().try_acquire_owned().map_err(|_| {
                Status::resource_exhausted(
                    "too many running commands or terminal sessions; close an unused session",
                )
            })?;
            let (input, receiver) = mpsc::channel(32);
            let session = Arc::new(Session {
                input,
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
                    },
                    screen: vt100::Parser::new_with_callbacks(
                        rows,
                        cols,
                        200,
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
            (session, true)
        };
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
            if !created {
                let screen = redraw(&state.screen);
                let _ = output.try_send(Ok(ExecEvent {
                    event: Some(exec_event::Event::Stdout(screen)),
                }));
            }
            state.output = Some(output.clone());
            state.info.rows = rows.into();
            state.info.cols = cols.into();
            state.screen.screen_mut().set_size(rows, cols);
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
            tokio::spawn(own_session(self.clone(), session, first, input, permit));
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
) {
    let name = first.session_name.clone();
    let (sender, mut output) = mpsc::channel(16);
    let worker = tokio::spawn(run_pty_command(first, ReceiverStream::new(input), sender));
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
                    state.screen.process(bytes);
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
    kitty: u16,
    stack: Vec<u16>,
    modify_keys: u16,
    focus: bool,
}
impl vt100::Callbacks for TerminalModes {
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        let value = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let second = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
        match (i1, command) {
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
}
fn redraw(parser: &vt100::Parser<TerminalModes>) -> Vec<u8> {
    let mut result = if parser.screen().alternate_screen() {
        b"\x1b[?1049h".to_vec()
    } else {
        b"\x1b[?1049l".to_vec()
    };
    result.extend(parser.screen().state_formatted());
    let modes = parser.callbacks();
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

#[cfg(test)]
mod tests {
    use super::*;
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
