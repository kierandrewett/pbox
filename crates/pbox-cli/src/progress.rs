//! Keep completed creation steps above the active spinner; diagnostics are opt-in.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::{
    thread,
    time::{Duration, Instant},
};

static VERBOSE: AtomicBool = AtomicBool::new(false);
pub fn set_verbose(value: bool) {
    VERBOSE.store(value, Ordering::Relaxed);
}
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

enum ProgressEvent {
    Phase(String),
    Finish,
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
            progress.sender = Some(sender);
            progress.worker = Some(thread::spawn(move || {
                let mut started = Instant::now();
                let mut phase: Option<String> = None;
                let mut frame = 0;
                let style = super::ui::stderr();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(120)) {
                        Ok(ProgressEvent::Phase(next)) => {
                            if let Some(previous) = &phase {
                                style.clear_progress_line();
                                style.completed_step(previous, started.elapsed().as_secs());
                            }
                            phase = Some(next);
                            started = Instant::now();
                            frame = 0;
                        }
                        Ok(ProgressEvent::Finish) => {
                            style.clear_progress_line();
                            if let Some(phase) = &phase {
                                style.completed_step(phase, started.elapsed().as_secs());
                            }
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            // Dropping during failure must not mark the current step complete.
                            style.clear_progress_line();
                            break;
                        }
                    }
                    if let Some(phase) = &phase {
                        let marker = ["|", "/", "-", "\\"][frame % 4];
                        style.spinner_frame(marker, phase, started.elapsed().as_secs());
                        frame += 1;
                    }
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
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
