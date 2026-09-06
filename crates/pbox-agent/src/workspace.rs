//! Minimal PID 1 for OCI workspaces. No distro service manager is started.
use anyhow::{Context, Result, ensure};
use nix::{
    libc,
    unistd::{Group, Uid, User},
};
use pbox_proto::agent::ExecRequest;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tonic::Status;

const IMAGE: &str = "/etc/pbox/image.json";
pub fn hostname() -> String {
    let mut name = [0u8; 256];
    unsafe {
        libc::gethostname(name.as_mut_ptr().cast(), name.len() - 1);
    }
    String::from_utf8_lossy(&name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())])
        .into_owned()
}
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

struct Identity {
    uid: u32,
    gid: u32,
    name: Option<String>,
    home: String,
    shell: String,
}

fn identity(value: &str) -> Result<Identity> {
    let (name, group) = value
        .split_once(':')
        .map_or((value, None), |(u, g)| (u, Some(g)));
    let numeric = name.parse::<u32>().ok();
    let user = match numeric {
        Some(uid) => User::from_uid(Uid::from_raw(uid))?,
        None => User::from_name(name)?,
    };
    ensure!(
        user.is_some() || numeric.is_some(),
        "image user does not exist: {name}"
    );
    let gid = match group {
        Some(group) => match group.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => Group::from_name(group)?
                .context("image group does not exist")?
                .gid
                .as_raw(),
        },
        None => user.as_ref().map_or(0, |u| u.gid.as_raw()),
    };
    Ok(Identity {
        uid: user
            .as_ref()
            .map_or_else(|| numeric.unwrap(), |u| u.uid.as_raw()),
        gid,
        name: user.as_ref().map(|u| u.name.clone()),
        home: user
            .as_ref()
            .map_or_else(|| "/".to_owned(), |u| u.dir.to_string_lossy().into_owned()),
        shell: user.as_ref().map_or_else(
            || "/bin/sh".to_owned(),
            |u| u.shell.to_string_lossy().into_owned(),
        ),
    })
}

pub fn resolve_request(mut request: ExecRequest) -> Result<ExecRequest, Status> {
    if !Path::new(IMAGE).exists() {
        if request.cwd.is_empty() {
            request.cwd = "/home/pbox".to_owned();
        }
        return Ok(request);
    }
    let config: Value =
        serde_json::from_slice(&fs::read(IMAGE).map_err(|e| Status::internal(e.to_string()))?)
            .map_err(|e| Status::internal(e.to_string()))?;
    resolve(&mut request, &config).map_err(|e| Status::invalid_argument(e.to_string()))?;
    Ok(request)
}

fn resolve(request: &mut ExecRequest, config: &Value) -> Result<()> {
    let development_default = Path::new("/etc/pbox/default-user").exists()
        && matches!(
            config["User"].as_str().unwrap_or(""),
            "" | "root" | "0" | "0:0" | "root:root"
        );
    let default_user = request.user.is_empty();
    if request.user.is_empty() {
        request.user = config["User"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_owned();
    }
    if default_user && development_default {
        request.user = "pbox".to_owned();
    }
    let user = identity(&request.user)?;
    if request.allocate_pty {
        request
            .env
            .entry("TERM".to_owned())
            .or_insert_with(|| "xterm-256color".to_owned());
    }
    if request.cwd.is_empty() {
        request.cwd = config["WorkingDir"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("/")
            .to_owned();
        if default_user && development_default && request.cwd == "/" {
            request.cwd = "/home/pbox".to_owned();
        }
    }
    let explicit_home = request.env.contains_key("HOME");
    for entry in strings(config, "Env") {
        if let Some((key, value)) = entry.split_once('=') {
            request
                .env
                .entry(key.to_owned())
                .or_insert(value.to_owned());
        }
    }
    if default_user && development_default {
        if !explicit_home {
            request.env.insert("HOME".to_owned(), user.home.clone());
        }
        request.env.insert("USER".to_owned(), "pbox".to_owned());
        request.env.insert("LOGNAME".to_owned(), "pbox".to_owned());
    }
    request
        .env
        .entry("HOME".to_owned())
        .or_insert(user.home.clone());
    request
        .env
        .entry("USER".to_owned())
        .or_insert_with(|| user.name.clone().unwrap_or_else(|| user.uid.to_string()));
    if request.argv == ["pbox:default"] {
        let mut argv = strings(config, "Entrypoint");
        argv.extend(strings(config, "Cmd"));
        if shell_command(&argv) {
            request.argv = argv;
        } else {
            request.argv = vec![user.shell.clone(), "-i".to_owned()];
        }
        ensure!(
            !request.argv[0].is_empty(),
            "image user has no shell; provide a command"
        );
    }
    Ok(())
}

fn shell_command(argv: &[String]) -> bool {
    argv.first()
        .and_then(|p| Path::new(p).file_name())
        .and_then(|p| p.to_str())
        .is_some_and(|s| matches!(s, "sh" | "bash" | "zsh" | "fish" | "ash" | "dash" | "ksh"))
        && argv.len() == 1
}

pub fn command(request: &ExecRequest) -> Result<std::process::Command> {
    use std::os::unix::process::CommandExt;
    let user = identity(&request.user)?;
    let gid = user.gid;
    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .env_clear()
        .envs(&request.env)
        .current_dir(&request.cwd);
    // Resolve supplementary groups before fork; NSS lookups are not fork-safe.
    let groups: Vec<libc::gid_t> = if let Some(name) = &user.name {
        let name = std::ffi::CString::new(name.as_str())?;
        nix::unistd::getgrouplist(&name, nix::unistd::Gid::from_raw(gid))?
            .iter()
            .map(|g| g.as_raw())
            .collect()
    } else {
        Vec::new()
    };
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(user.uid) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

fn log(path: &str) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?)
}

fn snapshot_boot() -> bool {
    fs::read_to_string("/etc/pbox-snapshot/source-host")
        .ok()
        .is_some_and(|s| s.trim() != hostname())
}

fn agent_files(snapshot: bool) -> (&'static str, &'static str) {
    if snapshot {
        (
            "/etc/pbox-snapshot/pbox-agent",
            "/etc/pbox-snapshot/agent-args.json",
        )
    } else {
        ("/usr/local/bin/pbox-agent", "/etc/pbox/agent-args.json")
    }
}

fn image_application(config: &Value) -> Result<Option<ExecRequest>> {
    let mut argv = strings(config, "Entrypoint");
    argv.extend(strings(config, "Cmd"));
    if argv.is_empty() || shell_command(&argv) {
        return Ok(None);
    }
    let mut request = ExecRequest {
        argv,
        user: config["User"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_owned(),
        cwd: config["WorkingDir"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("/")
            .to_owned(),
        ..Default::default()
    };
    resolve(&mut request, config)?;
    Ok(Some(request))
}

pub fn service(application: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    if application {
        if snapshot_boot() {
            return Ok(());
        }
        let config: Value = serde_json::from_slice(&fs::read(IMAGE)?)?;
        let Some(request) = image_application(&config)? else {
            return Ok(());
        };
        return Err(command(&request)?.exec()).context("start image application");
    }
    let (binary, arguments) = agent_files(snapshot_boot());
    let args: Vec<String> = serde_json::from_slice(&fs::read(arguments)?)?;
    Err(Command::new(binary).args(args).exec()).context("start managed guest agent")
}

fn configure_systemd(root: &Path) -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let units = root.join("etc/systemd/system");
    fs::create_dir_all(units.join("multi-user.target.wants"))?;
    // The service wrapper selects clone credentials itself, unlike legacy units.
    let _ = fs::remove_file(units.join("pbox-agent.service.d/90-snapshot.conf"));
    let write = |name: &str, content: &str| -> Result<()> {
        let temporary = units.join(format!(".{name}.pbox-tmp"));
        fs::write(&temporary, content)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))?;
        fs::rename(temporary, units.join(name))?;
        Ok(())
    };
    write(
        "pbox-agent.service",
        "[Unit]\nDescription=pbox guest agent\nAfter=local-fs.target pbox-network.service\nWants=pbox-network.service\n[Service]\nExecStart=/usr/local/bin/pbox-agent --workspace-service\nRestart=always\nRestartSec=2\n[Install]\nWantedBy=multi-user.target\n",
    )?;
    write(
        "pbox-network.service",
        "[Unit]\nDescription=pbox guest networking\nBefore=pbox-agent.service\n[Service]\nEnvironment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\nExecStart=/usr/bin/env dhcpcd --nodev -B -e resolvconf=/nonexistent -j /var/log/pbox-network.log -f /etc/pbox/dhcpcd.conf\nRestart=always\nRestartSec=2\n[Install]\nWantedBy=multi-user.target\n",
    )?;
    write(
        "pbox-application.service",
        "[Unit]\nDescription=OCI image application\nAfter=network-online.target\nWants=network-online.target\n[Service]\nExecStart=/usr/local/bin/pbox-agent --workspace-application\n[Install]\nWantedBy=multi-user.target\n",
    )?;
    for name in [
        "pbox-agent.service",
        "pbox-network.service",
        "pbox-application.service",
    ] {
        let link = units.join("multi-user.target.wants").join(name);
        let _ = fs::remove_file(&link);
        symlink(format!("../{name}"), link)?;
    }
    // pbox owns guest addressing. Competing managers and interactive first-boot
    // setup must not block unattended boot or replace its DHCP configuration.
    for name in [
        "systemd-firstboot.service",
        "systemd-networkd.service",
        "NetworkManager.service",
        "networking.service",
        "dhcpcd.service",
    ] {
        let link = units.join(name);
        let _ = fs::remove_file(&link);
        symlink("/dev/null", link)?;
    }
    Ok(())
}

pub fn supervise() -> Result<()> {
    ensure!(
        std::process::id() == 1,
        "workspace supervisor must run as container PID 1"
    );
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGPWR, stop as *const () as libc::sighandler_t);
    }
    fs::create_dir_all("/run")?;
    fs::create_dir_all("/var/log")?;
    // LXC leaves loopback down when no distro init runs. The relay terminates
    // at the agent's loopback listener, so bring it up before launching children.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        ensure!(
            fd >= 0,
            "open network control socket: {}",
            std::io::Error::last_os_error()
        );
        let mut interface: libc::ifreq = std::mem::zeroed();
        interface.ifr_name[0] = b'l' as libc::c_char;
        interface.ifr_name[1] = b'o' as libc::c_char;
        let get = libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut interface);
        interface.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        let set = if get == 0 {
            libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &interface)
        } else {
            -1
        };
        let error = std::io::Error::last_os_error();
        libc::close(fd);
        ensure!(set == 0, "bring loopback up: {error}");
    }
    let snapshot = snapshot_boot();
    if snapshot {
        ensure!(
            Command::new("/bin/sh")
                .arg("/etc/pbox-snapshot/sanitize")
                .status()?
                .success(),
            "snapshot sanitization failed"
        );
    }
    let (binary, arguments) = agent_files(snapshot);
    if let Some(systemd) = ["/usr/lib/systemd/systemd", "/lib/systemd/systemd"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
    {
        configure_systemd(Path::new("/"))?;
        use std::os::unix::process::CommandExt;
        return Err(Command::new(systemd)
            .args(["--system", "--unit=multi-user.target"])
            .env("container", "lxc")
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .exec())
        .context("replace bootstrap PID 1 with systemd");
    }
    let args: Vec<String> = serde_json::from_slice(&fs::read(arguments)?)?;
    let mut agent = None;
    let mut network = None;
    let mut next_agent = Instant::now();
    let mut next_network = Instant::now();
    let mut application = None;
    let config: Value = serde_json::from_slice(&fs::read(IMAGE)?)?;
    if !snapshot && let Some(request) = image_application(&config)? {
        application = Some(command(&request)?.stdin(Stdio::null()).spawn()?.id());
    }
    while !STOP.load(Ordering::Relaxed) {
        if agent.is_none() && Instant::now() >= next_agent {
            agent = Some(
                Command::new(binary)
                    .stdout(log("/var/log/pbox-agent.log")?)
                    .stderr(log("/var/log/pbox-agent.log")?)
                    .args(&args)
                    .stdin(Stdio::null())
                    .spawn()?
                    .id(),
            );
        }
        if network.is_none() && Instant::now() >= next_network {
            network = Some(
                Command::new("dhcpcd")
                    // PVE's minimal init PATH need not include Alpine's /sbin.
                    .env(
                        "PATH",
                        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                    )
                    .args([
                        "--nodev",
                        "-B",
                        "-e",
                        "resolvconf=/nonexistent",
                        "-j",
                        "/var/log/pbox-network.log",
                        "-f",
                        "/etc/pbox/dhcpcd.conf",
                    ])
                    .stdin(Stdio::null())
                    .spawn()?
                    .id(),
            );
        }
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if Some(pid as u32) == agent {
                agent = None;
                next_agent = Instant::now() + Duration::from_secs(2);
            }
            if Some(pid as u32) == network {
                network = None;
                next_network = Instant::now() + Duration::from_secs(2);
            }
            if Some(pid as u32) == application {
                STOP.store(true, Ordering::Relaxed);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(-1, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if pid == -1 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(-1, libc::SIGKILL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn systemd_setup_is_repeatable_and_enables_real_services() {
        let root = std::env::temp_dir().join(format!("pbox-init-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        configure_systemd(&root).unwrap();
        configure_systemd(&root).unwrap();
        let units = root.join("etc/systemd/system");
        assert_eq!(
            fs::read_link(units.join("systemd-firstboot.service")).unwrap(),
            Path::new("/dev/null")
        );
        assert!(
            fs::read_to_string(units.join("pbox-agent.service"))
                .unwrap()
                .contains("--workspace-service")
        );
        assert_eq!(
            fs::read_link(units.join("multi-user.target.wants/pbox-network.service")).unwrap(),
            Path::new("../pbox-network.service")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn numeric_identity_does_not_require_a_passwd_entry() {
        let user = identity("34567:45678").unwrap();
        assert_eq!(user.uid, 34567);
        assert_eq!(user.gid, 45678);
    }

    #[test]
    fn explicit_command_user_and_directory_override_image_defaults() {
        let mut request = ExecRequest {
            argv: vec!["true".into()],
            user: "root".into(),
            cwd: "/tmp".into(),
            allocate_pty: true,
            ..Default::default()
        };
        resolve(&mut request, &serde_json::json!({"User":"does-not-exist", "WorkingDir":"/elsewhere", "Cmd":["bash"]})).unwrap();
        assert_eq!(request.user, "root");
        assert_eq!(request.cwd, "/tmp");
        assert_eq!(request.argv, ["true"]);
        assert_eq!(request.env["TERM"], "xterm-256color");
    }

    #[test]
    fn defaults_preserve_image_environment_and_explicit_overrides() {
        let config = serde_json::json!({"User":"root","WorkingDir":"/work","Env":["EDITOR=vim","HOME=/custom"],"Cmd":["/usr/bin/bash"]});
        let mut request = ExecRequest {
            argv: vec!["pbox:default".into()],
            ..Default::default()
        };
        request.env.insert("EDITOR".into(), "nano".into());
        resolve(&mut request, &config).unwrap();
        assert_eq!(request.argv, ["/usr/bin/bash"]);
        assert_eq!(request.cwd, "/work");
        assert_eq!(request.user, "root");
        assert_eq!(request.env["EDITOR"], "nano");
        assert_eq!(request.env["HOME"], "/custom");
    }
}
