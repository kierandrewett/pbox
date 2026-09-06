//! The CLI reads snapshots only. PVE discovery and probes belong to pboxd.
use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Snapshot {
    updated: u64,
    pub records: Vec<BoxRecord>,
    pub refresh_failed: bool,
}

impl Snapshot {
    pub fn age(&self) -> Duration {
        Duration::from_secs(now().saturating_sub(self.updated))
    }
}

#[derive(Serialize, Deserialize)]
struct Deletion {
    node: String,
    upid: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn directory(config: &Path) -> Result<PathBuf> {
    let absolute = fs::canonicalize(config).unwrap_or(std::env::current_dir()?.join(config));
    let mut hash = Sha256::new();
    hash.update(absolute.as_os_str().as_encoded_bytes());
    hash.update(fs::read(config).unwrap_or_default());
    // Environment overrides select a different inventory too, without exposing secrets.
    let mut overrides: Vec<_> = std::env::vars()
        .filter(|(k, _)| k.starts_with("PBOX_"))
        .collect();
    overrides.sort();
    hash.update(serde_json::to_vec(&overrides)?);
    Ok(dirs::data_local_dir()
        .context("resolve pbox state directory")?
        .join("pbox/inventory-v2")
        .join(format!("{:x}", hash.finalize())))
}

fn private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?)
}

fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = private_file(&temporary)?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn read_snapshot(dir: &Path) -> Result<Option<Snapshot>> {
    match fs::read(dir.join("inventory.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn read(config: &Path) -> Result<Option<Snapshot>> {
    let dir = directory(config)?;
    read_at(&dir)
}

fn read_at(dir: &Path) -> Result<Option<Snapshot>> {
    let Some(mut snapshot) = read_snapshot(dir)? else {
        return Ok(None);
    };
    for record in &mut snapshot.records {
        if dir.join(format!("{}.delete", record.id)).exists() {
            record.state = "deleting".to_owned();
            record.ping = None;
        }
    }
    Ok(Some(snapshot))
}

pub(super) fn mark_deleting(
    config: &Path,
    record: &BoxRecord,
    task: &PveTaskResponse,
) -> Result<()> {
    let dir = directory(config)?;
    fs::create_dir_all(&dir)?;
    write_atomic(
        &dir.join(format!("{}.delete", record.id)),
        &Deletion {
            node: record.node.clone(),
            upid: task.upid.clone(),
        },
    )
}

pub(super) fn ensure_daemon(config: &Path) {
    let start = || -> Result<()> {
        let dir = directory(config)?;
        fs::create_dir_all(&dir)?;
        let lock = private_file(&dir.join("daemon.lock"))?;
        if lock.try_lock().is_err() {
            return Ok(());
        }
        drop(lock);
        // Invalid test/setup configurations must not leave idle daemons behind.
        let settings = load_config(&ConfigStore::new(config))?;
        let _ = client_from_config(&settings)?;
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
            .arg("daemon")
            .arg("--config")
            .arg(config)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    };
    let _ = start();
}

pub(super) fn run_daemon(config_path: &Path) -> Result<RunOutcome> {
    let dir = directory(config_path)?;
    fs::create_dir_all(&dir)?;
    let lock = private_file(&dir.join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(RunOutcome::Success);
    }
    let executable = std::env::current_exe()?;
    let version = fs::metadata(&executable)?.modified()?;
    loop {
        if directory(config_path)? != dir
            || fs::metadata(&executable).and_then(|m| m.modified()).ok() != Some(version)
        {
            return Ok(RunOutcome::Success);
        }
        let refresh = || -> Result<Snapshot> {
            let config = load_config(&ConfigStore::new(config_path))?;
            let client = client_from_config(&config)?;
            let mut records = discover_boxes(&client)?;
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().is_some_and(|ext| ext == "delete") {
                    let deletion: Deletion = serde_json::from_slice(&fs::read(&path)?)?;
                    if client
                        .get_task_status(&deletion.node, &deletion.upid)
                        .is_ok_and(|task| task.status == "stopped")
                    {
                        fs::remove_file(path)?;
                    } else if let Some(record) = records
                        .iter_mut()
                        .find(|r| path.file_stem().is_some_and(|id| id == r.id.as_str()))
                    {
                        record.state = "deleting".to_owned();
                        record.ping = None;
                    }
                }
            }
            // Keep the last probe result while publishing fresh PVE lifecycle state.
            // Only the background worker probes; readers never wait for it.
            let mut pending = records.clone();
            if let Some(previous) = read_snapshot(&dir)? {
                for record in &mut pending {
                    if record.state == "running"
                        && let Some(old) = previous.records.iter().find(|old| old.id == record.id)
                        && matches!(old.state.as_str(), "running" | "disconnected")
                    {
                        record.state.clone_from(&old.state);
                        record.ping.clone_from(&old.ping);
                    }
                }
            }
            write_atomic(
                &dir.join("inventory.json"),
                &Snapshot {
                    updated: now(),
                    records: pending,
                    refresh_failed: false,
                },
            )?;
            probe_box_agents(&config, &mut records);
            Ok(Snapshot {
                updated: now(),
                records,
                refresh_failed: false,
            })
        };
        match refresh() {
            Ok(snapshot) => write_atomic(&dir.join("inventory.json"), &snapshot)?,
            Err(_) => {
                if let Some(mut snapshot) = read_snapshot(&dir)? {
                    snapshot.refresh_failed = true;
                    write_atomic(&dir.join("inventory.json"), &snapshot)?;
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_read_does_not_wait_for_daemon_lock_and_deletion_overrides_ping() {
        let dir = std::env::temp_dir().join(format!("pbox-inventory-{}", PboxId::generate()));
        fs::create_dir_all(&dir).unwrap();
        let lock = private_file(&dir.join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(read_at(&dir).unwrap().is_none());
        let record: BoxRecord = serde_json::from_value(serde_json::json!({
            "id":"pbx_test1234", "vmid":9000, "state":"disconnected", "node":"test",
            "ip":null, "ipv6":null, "name":"test", "recipes":[], "capabilities":[], "ping":"timeout"
        }))
        .unwrap();
        write_atomic(
            &dir.join("inventory.json"),
            &Snapshot {
                updated: now(),
                records: vec![record],
                refresh_failed: false,
            },
        )
        .unwrap();
        write_atomic(
            &dir.join("pbx_test1234.delete"),
            &Deletion {
                node: "test".into(),
                upid: "test".into(),
            },
        )
        .unwrap();
        let snapshot = read_at(&dir).unwrap().unwrap();
        assert_eq!(snapshot.records[0].state, "deleting");
        assert_eq!(snapshot.records[0].ping, None);
        drop(lock);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configs_do_not_share_cached_inventory() {
        assert_ne!(
            directory(Path::new("/tmp/pbox-config-one")).unwrap(),
            directory(Path::new("/tmp/pbox-config-two")).unwrap()
        );
    }
}
