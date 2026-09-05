//! One terminal status line for normal creation; diagnostic output is opt-in.
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

pub struct CreationProgress {
    sender: Option<mpsc::Sender<String>>,
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
            let (sender, receiver) = mpsc::channel::<String>();
            progress.sender = Some(sender);
            progress.worker = Some(thread::spawn(move || {
                let started = Instant::now();
                let mut phase = "Connecting to Proxmox".to_owned();
                let mut frame = 0;
                loop {
                    match receiver.recv_timeout(Duration::from_millis(120)) {
                        Ok(next) => phase = next,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    let marker = ["|", "/", "-", "\\"][frame % 4];
                    super::ui::stderr().spinner_frame(marker, &phase, started.elapsed().as_secs());
                    frame += 1;
                }
                super::ui::stderr().clear_progress_line();
            }));
        }
        progress.phase("Connecting to Proxmox");
        progress
    }

    pub fn phase(&self, text: &str) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(text.to_owned());
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
