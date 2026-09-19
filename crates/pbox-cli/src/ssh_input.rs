//! Local terminal control must not depend on an agent or a writable network queue.
use super::{ExecInput, sessions};
use anyhow::{Result, anyhow};
use std::{
    io::{self, IsTerminal, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

pub struct Input {
    pub data: Option<mpsc::Receiver<ExecInput>>,
    pub control: oneshot::Receiver<Result<()>>,
    pub ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Input {
    pub fn start(read_only: bool) -> Self {
        let (sender, data) = mpsc::channel(32);
        let (control, receiver) = oneshot::channel();
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_ready = ready.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let result = pump(sender, &thread_ready, &thread_stop, read_only);
            // EOF is handled by the remote stream; it is not a local detach.
            if let Some(result) = result {
                let _ = control.send(result);
            } else {
                while !thread_stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
        });
        Self {
            data: Some(data),
            control: receiver,
            ready,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(unix)]
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub async fn cancellable<T>(
    control: &mut oneshot::Receiver<Result<()>>,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<Option<T>> {
    tokio::select! {
        biased;
        result = control => { result??; Ok(None) },
        result = operation => result.map(Some),
    }
}

fn pump(
    sender: mpsc::Sender<ExecInput>,
    ready: &AtomicBool,
    stop: &AtomicBool,
    read_only: bool,
) -> Option<Result<()>> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut stdin = stdin.lock();
    let mut parser = if read_only {
        sessions::TerminalInput::read_only()
    } else {
        sessions::TerminalInput::default()
    };
    let mut buffer = [0; 8192];
    while !stop.load(Ordering::Acquire) {
        if !interactive && !ready.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }
        #[cfg(unix)]
        {
            let mut poll = nix::libc::pollfd {
                fd: 0,
                events: nix::libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { nix::libc::poll(&mut poll, 1, 30) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Some(Err(error.into()));
            }
            if result == 0 {
                if parser.pending() {
                    let bytes = parser.flush();
                    if ready.load(Ordering::Acquire)
                        && sender.try_send(ExecInput::Data(bytes)).is_err()
                    {
                        return Some(Err(anyhow!("terminal input queue is full or closed")));
                    }
                }
                continue;
            }
        }
        let count = match stdin.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Some(Err(error.into())),
        };
        if count == 0 {
            if sender.blocking_send(ExecInput::Eof).is_err() {
                return Some(Err(anyhow!("terminal input queue is full or closed")));
            }
            return None;
        }
        let (bytes, detach) = if interactive {
            parser.push(&buffer[..count])
        } else {
            (buffer[..count].to_vec(), false)
        };
        // Detach has its own channel and never waits behind data or a resize.
        if detach {
            return Some(Ok(()));
        }
        if !interactive {
            if sender.blocking_send(ExecInput::Data(bytes)).is_err() {
                return Some(Err(anyhow!("terminal input queue closed")));
            }
            continue;
        }
        // Never replay keys typed before a shell was available.
        if ready.load(Ordering::Acquire)
            && !bytes.is_empty()
            && sender.try_send(ExecInput::Data(bytes)).is_err()
        {
            return Some(Err(anyhow!("terminal input queue is full or closed")));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detach_cancels_a_connection_that_never_completes() {
        let (sender, mut receiver) = oneshot::channel();
        sender.send(Ok(())).unwrap();
        let result = cancellable(&mut receiver, std::future::pending::<Result<()>>())
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn detach_cancels_a_full_network_queue() {
        let (sender, _receiver) = mpsc::channel(1);
        sender.send(ExecInput::Eof).await.unwrap();
        let (detach, mut control) = oneshot::channel();
        let operation = async {
            sender.send(ExecInput::Detach).await?;
            Ok(())
        };
        detach.send(Ok(())).unwrap();
        assert!(
            cancellable(&mut control, operation)
                .await
                .unwrap()
                .is_none()
        );
    }
}
