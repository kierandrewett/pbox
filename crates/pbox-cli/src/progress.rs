//! Keep completed creation steps above the active spinner; diagnostics are opt-in.
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::{thread, time::Duration};

static VERBOSE: AtomicBool = AtomicBool::new(false);
pub fn set_verbose(value: bool) {
    VERBOSE.store(value, Ordering::Relaxed);
}
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

static DETAILS: Mutex<Option<mpsc::Sender<ProgressEvent>>> = Mutex::new(None);

pub(crate) fn has_details() -> bool {
    DETAILS.lock().is_ok_and(|sender| sender.is_some())
}
fn detail(event: ProgressEvent) {
    if let Ok(sender) = DETAILS.lock()
        && let Some(sender) = sender.as_ref()
    {
        let _ = sender.send(event);
    }
}
pub(crate) fn substep(action: &str) {
    if verbose() {
        super::ui::stderr().diagnostic(action);
    }
    detail(ProgressEvent::Substep(action.to_owned()));
}
pub(crate) fn substep_done(action: &str, elapsed: u64, success: bool) {
    if verbose() {
        super::ui::stderr().diagnostic(&format!(
            "{action}: {} ({elapsed}s)",
            if success { "done" } else { "failed" }
        ));
    }
    detail(ProgressEvent::SubstepDone(
        action.to_owned(),
        elapsed,
        success,
    ));
}
pub(crate) fn log(line: &str) {
    if verbose() {
        super::ui::stderr().diagnostic(line);
    }
    detail(ProgressEvent::Log(line.to_owned()));
}

enum ProgressEvent {
    Phase(String),
    Finish,
    Substep(String),
    SubstepDone(String, u64, bool),
    Log(String),
}

pub struct CreationProgress {
    sender: Option<mpsc::Sender<ProgressEvent>>,
    worker: Option<thread::JoinHandle<()>>,
    visible: bool,
}

impl CreationProgress {
    pub fn new(json: bool) -> Self {
        let mut progress = Self {
            sender: None,
            worker: None,
            visible: !json,
        };
        if !json && !verbose() && super::ui::stderr().can_animate() {
            let (sender, receiver) = mpsc::channel::<ProgressEvent>();
            if let Ok(mut details) = DETAILS.lock() {
                *details = Some(sender.clone());
            }
            progress.sender = Some(sender);
            progress.worker = Some(thread::spawn(move || {
                let mut display = super::ui::CreationDisplay::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(120)) {
                        Ok(ProgressEvent::Phase(next)) => display.phase(next),
                        Ok(ProgressEvent::Substep(action)) => display.substep(action),
                        Ok(ProgressEvent::SubstepDone(action, elapsed, success)) => {
                            display.substep_done(action, elapsed, success)
                        }
                        Ok(ProgressEvent::Log(line)) => display.log(line),
                        Ok(ProgressEvent::Finish) => {
                            display.finish(true);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            display.finish(false);
                            break;
                        }
                    }
                    display.tick();
                }
            }));
        }
        progress.phase("Connecting to Proxmox");
        progress
    }

    pub fn finish(self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ProgressEvent::Finish);
        }
        // Drop joins the renderer before the command prints its final result.
    }

    pub fn phase(&self, text: &str) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ProgressEvent::Phase(text.to_owned()));
        } else if self.visible {
            super::ui::stderr().progress(&format!("{text}..."));
        }
    }
}

impl Drop for CreationProgress {
    fn drop(&mut self) {
        if self.sender.is_some()
            && let Ok(mut sender) = DETAILS.lock()
        {
            sender.take();
        }
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
