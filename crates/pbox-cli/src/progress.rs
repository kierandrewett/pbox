//! Timed operation stages, live task logs and explicit failure/recovery transitions.
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
fn detail(event: ProgressEvent) -> bool {
    if let Ok(sender) = DETAILS.lock()
        && let Some(sender) = sender.as_ref()
    {
        return sender.send(event).is_ok();
    }
    false
}
pub(crate) fn substep(action: &str) {
    if !detail(ProgressEvent::Substep(action.to_owned())) && verbose() {
        super::ui::stderr().diagnostic(action);
    }
}
pub(crate) fn substep_done(action: &str, elapsed: u64, success: bool) {
    if !detail(ProgressEvent::SubstepDone(
        action.to_owned(),
        elapsed,
        success,
    )) && verbose()
    {
        super::ui::stderr().diagnostic(&format!(
            "{action}: {} ({elapsed}s)",
            if success { "done" } else { "failed" }
        ));
    }
}
pub(crate) fn log(line: &str) {
    if !detail(ProgressEvent::Log(line.to_owned())) && verbose() {
        super::ui::stderr().diagnostic(line);
    }
}

enum ProgressEvent {
    Phase(String),
    PhaseFailed,
    Finish,
    Substep(String),
    SubstepDone(String, u64, bool),
    Log(String),
}

pub struct CreationProgress {
    sender: Option<mpsc::Sender<ProgressEvent>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CreationProgress {
    pub fn new(json: bool) -> Self {
        Self::start(json, "Connecting to Proxmox")
    }

    pub fn start(json: bool, initial_phase: &str) -> Self {
        let mut progress = Self {
            sender: None,
            worker: None,
        };
        if !json {
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
                        Ok(ProgressEvent::PhaseFailed) => display.finish(false),
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
        progress.phase(initial_phase);
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
        }
    }

    pub fn fail_phase(&self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ProgressEvent::PhaseFailed);
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

#[cfg(test)]
mod tests {
    use super::*;

    // Separate processes keep the renderer's global event channel and output
    // mode isolated from the rest of the test suite.
    #[test]
    fn progress_preserves_failures_logs_heartbeats_and_json_silence() {
        for mode in ["plain", "failure", "json", "verbose", "heartbeat"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "progress::tests::snapshot_progress_preview",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PBOX_PROGRESS_PREVIEW", mode)
                .env("TERM", "dumb")
                .output()
                .unwrap();
            assert!(output.status.success());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(!stderr.contains('\x1b'), "{mode}: {stderr:?}");
            if mode == "json" {
                assert!(stderr.is_empty(), "{stderr}");
                continue;
            }
            assert_eq!(
                stderr
                    .matches("Total transferred file size: 5287616645 bytes")
                    .count(),
                1
            );
            assert!(stderr.contains("future PVE log format"));
            if mode == "failure" {
                assert!(stderr.contains("x 3/5 Copying disks (source stopped)"));
                assert!(!stderr.contains("ok 3/5 Copying disks"));
                assert!(stderr.contains("ok Recovering source after failure"));
            } else {
                assert!(stderr.contains("ok 3/5 Copying disks (source stopped)"));
            }
            if mode == "heartbeat" {
                assert!(stderr.lines().any(|line| {
                    line.contains("3/5 Copying disks (source stopped)")
                        && line.ends_with("s elapsed)")
                }));
            }
        }
    }

    #[test]
    #[ignore = "renderer fixture and manual terminal preview"]
    fn snapshot_progress_preview() {
        let mode = std::env::var("PBOX_PROGRESS_PREVIEW").unwrap_or_default();
        let json = mode == "json";
        super::super::ui::configure(super::super::ColorChoice::Auto, json);
        set_verbose(mode == "verbose");
        if !json {
            super::super::ui::snapshot_heading("dev-base", "pbx_example");
        }
        let progress = CreationProgress::start(json, "1/5 Preparing source");
        substep("Waiting for source agent");
        progress.phase("2/5 Stopping source");
        progress.phase("3/5 Copying disks (source stopped)");
        substep("Copying on pve; PVE may only report final totals");
        log("create full clone of mountpoint rootfs (local:9000/disk)");
        if mode == "heartbeat" || mode == "terminal" {
            thread::sleep(Duration::from_millis(5300));
        }
        log("\x1b[2Jfuture PVE log format");
        log("Total transferred file size: 5287616645 bytes");
        if mode == "failure" {
            progress.fail_phase();
            progress.phase("Recovering source after failure");
        } else {
            progress.phase("4/5 Finalising snapshot");
            progress.phase("5/5 Restoring source");
        }
        substep("Waiting for source agent");
        progress.finish();
    }
}
