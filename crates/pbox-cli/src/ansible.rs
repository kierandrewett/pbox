use crate::recipes::{Recipe, RecipeCatalog, RecipeKind};
use anyhow::{Context, Result, anyhow, bail};
use pbox_core::PboxId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CONNECTION_PLUGIN_NAME: &str = "pbox_agent";
const BRIDGE_PROTOCOL_VERSION: u32 = 1;
const MAX_BRIDGE_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct AnsibleRun {
    pub recipe: String,
    pub box_id: String,
    pub repository: String,
    pub revision: String,
}

#[derive(Debug)]
pub struct RecipeApplyResult {
    pub run: AnsibleRun,
    pub cleanup_error: Option<anyhow::Error>,
}

struct AnsibleInvocation<'a> {
    config_path: &'a Path,
    pbox_binary: &'a Path,
    repository_root: &'a Path,
    operation_directory: &'a Path,
    plugin_directory: &'a Path,
    inventory: &'a Path,
    box_id: &'a str,
}

pub fn apply_recipes(
    config_path: &Path,
    pbox_binary: &Path,
    repository_root: &Path,
    catalog: &RecipeCatalog,
    recipes: &[&Recipe],
    box_id: &str,
    json: bool,
) -> Result<RecipeApplyResult> {
    validate_box_id(box_id)?;
    anyhow::ensure!(!recipes.is_empty(), "at least one recipe is required");
    let recipe_label = recipes
        .iter()
        .map(|recipe| recipe.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !json {
        crate::ui::recipe_heading(&recipe_label, box_id);
    }
    if !repository_root.is_dir() {
        bail!(
            "recipe repository directory does not exist: {}",
            repository_root.display()
        );
    }
    let operation_directory = operation_directory(&recipe_label, box_id)?;
    let result = (|| -> Result<()> {
        let plugin_directory = operation_directory.join("connection_plugins");
        fs::create_dir_all(&plugin_directory).with_context(|| {
            format!(
                "create Ansible plugin directory {}",
                plugin_directory.display()
            )
        })?;
        set_mode(&operation_directory, 0o700)?;
        write_file(
            &plugin_directory.join(format!("{CONNECTION_PLUGIN_NAME}.py")),
            connection_plugin_source(),
            0o600,
        )?;

        let inventory = operation_directory.join("inventory");
        write_file(
            &inventory,
            &format!(
                "[pbox]\n{box_id} ansible_connection={CONNECTION_PLUGIN_NAME} ansible_user=root ansible_python_interpreter=/usr/bin/python3\n"
            ),
            0o600,
        )?;

        let preflight = operation_directory.join("preflight.yml");
        write_file(&preflight, preflight_playbook(), 0o600)?;
        let invocation = AnsibleInvocation {
            config_path,
            pbox_binary,
            repository_root,
            operation_directory: &operation_directory,
            plugin_directory: &plugin_directory,
            inventory: &inventory,
            box_id,
        };
        run_ansible(&invocation, &preflight, json)
            .context("ensure the guest has a Python interpreter for Ansible")?;

        let combined = operation_directory.join("recipes.yml");
        let mut combined_playbook = String::new();
        for (index, recipe) in recipes.iter().enumerate() {
            let source_path = repository_path(repository_root, Path::new(&recipe.path))?;
            let playbook = match recipe.kind {
                RecipeKind::Playbook => source_path,
                RecipeKind::Role => {
                    let role_name = role_name(repository_root, &recipe.path)?;
                    let wrapper = operation_directory.join(format!("recipe-{index}.yml"));
                    write_file(&wrapper, &role_playbook(&role_name), 0o600)?;
                    wrapper
                }
            };
            let path = playbook.to_string_lossy().replace('\'', "''");
            combined_playbook.push_str(&format!("- import_playbook: '{path}'\n"));
        }
        write_file(&combined, &combined_playbook, 0o600)?;
        run_ansible(&invocation, &combined, json).with_context(|| {
            format!(
                "apply recipe{} {} to box {box_id}",
                if recipes.len() == 1 { "" } else { "s" },
                recipe_label
            )
        })?;
        Ok(())
    })();
    let cleanup_result = fs::remove_dir_all(&operation_directory).with_context(|| {
        format!(
            "remove recipe operation directory {}",
            operation_directory.display()
        )
    });
    match result {
        Err(error) => {
            if let Err(cleanup_error) = cleanup_result {
                return Err(error.context(cleanup_error));
            }
            Err(error)
        }
        Ok(()) => Ok(RecipeApplyResult {
            run: AnsibleRun {
                recipe: recipe_label,
                box_id: box_id.to_owned(),
                repository: catalog.repository.clone(),
                revision: catalog.revision.clone(),
            },
            cleanup_error: cleanup_result.err(),
        }),
    }
}

#[derive(Debug, Deserialize)]
struct BridgeRequest {
    protocol: u32,
    box_id: String,
    #[serde(flatten)]
    operation: BridgeOperation,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BridgeOperation {
    Exec {
        argv: Vec<String>,
        cwd: String,
        env: Vec<String>,
        user: String,
    },
    PutFile {
        source: String,
        destination: String,
    },
    GetFile {
        source: String,
        destination: String,
    },
}

#[cfg(unix)]
#[derive(Clone)]
struct BridgeContext {
    config_path: PathBuf,
    pbox_binary: PathBuf,
    box_id: String,
}

#[cfg(unix)]
struct BridgeHandle {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

#[cfg(unix)]
impl BridgeHandle {
    fn start(
        invocation: &AnsibleInvocation<'_>,
        config_path: PathBuf,
        pbox_binary: PathBuf,
    ) -> Result<Self> {
        // Linux limits Unix-domain socket paths to SUN_LEN bytes. Operation
        // directories intentionally include the recipe, box and operation
        // identifiers, so putting the bridge below them can exceed that
        // limit before Ansible has a chance to connect.
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read system clock for Ansible bridge socket")?
            .as_nanos();
        let socket_path =
            std::env::temp_dir().join(format!("pbox-b-{}-{stamp}.sock", std::process::id()));
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "remove stale Ansible bridge socket {}",
                        socket_path.display()
                    )
                });
            }
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind Ansible bridge socket {}", socket_path.display()))?;
        set_mode(&socket_path, 0o600)?;
        listener
            .set_nonblocking(true)
            .context("configure Ansible bridge socket")?;

        let stop = Arc::new(AtomicBool::new(false));
        let context = BridgeContext {
            config_path,
            pbox_binary,
            box_id: invocation.box_id.to_owned(),
        };
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || bridge_loop(listener, thread_stop, context));
        Ok(Self {
            socket_path,
            stop,
            thread: Some(thread),
        })
    }

    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let thread = self
            .thread
            .take()
            .ok_or_else(|| anyhow!("Ansible bridge thread was already stopped"))?;
        thread
            .join()
            .map_err(|_| anyhow!("Ansible bridge thread panicked"))??;
        fs::remove_file(&self.socket_path).with_context(|| {
            format!(
                "remove Ansible bridge socket {}",
                self.socket_path.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(unix)]
fn bridge_loop(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    context: BridgeContext,
) -> Result<()> {
    let mut clients = Vec::new();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let client_context = context.clone();
                clients.push(thread::spawn(move || {
                    let _ = handle_bridge_client(stream, client_context);
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("accept Ansible bridge connection"),
        }
    }
    for client in clients {
        client
            .join()
            .map_err(|_| anyhow!("Ansible bridge client thread panicked"))?;
    }
    Ok(())
}

#[cfg(unix)]
fn handle_bridge_client(mut stream: UnixStream, context: BridgeContext) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .context("set Ansible bridge read timeout")?;
    let response = match read_bridge_line(&mut stream) {
        Ok(bytes) => match serde_json::from_slice::<BridgeRequest>(&bytes) {
            Ok(request) => handle_bridge_request(request, &context),
            Err(error) => Err(anyhow!("invalid Ansible bridge request: {error}")),
        },
        Err(error) => Err(anyhow!("read Ansible bridge request: {error}")),
    };
    let response = match response {
        Ok(value) => value,
        Err(error) => serde_json::json!({
            "protocol": BRIDGE_PROTOCOL_VERSION,
            "error": error.to_string(),
        }),
    };
    let mut encoded = serde_json::to_vec(&response).context("encode Ansible bridge response")?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .context("write Ansible bridge response")?;
    stream.flush().context("flush Ansible bridge response")?;
    Ok(())
}

#[cfg(unix)]
fn read_bridge_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let count = stream.read(&mut byte)?;
        if count == 0 {
            break;
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() > MAX_BRIDGE_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ansible bridge request exceeds the size limit",
            ));
        }
    }
    Ok(line)
}

#[cfg(unix)]
fn handle_bridge_request(
    request: BridgeRequest,
    context: &BridgeContext,
) -> Result<serde_json::Value> {
    if request.protocol != BRIDGE_PROTOCOL_VERSION {
        bail!(
            "unsupported Ansible bridge protocol {}; expected {}",
            request.protocol,
            BRIDGE_PROTOCOL_VERSION
        );
    }
    if request.box_id != context.box_id {
        bail!(
            "Ansible bridge request targeted {}; expected {}",
            request.box_id,
            context.box_id
        );
    }
    match request.operation {
        BridgeOperation::Exec {
            argv,
            cwd,
            env,
            user,
        } => bridge_exec(context, argv, cwd, env, user),
        BridgeOperation::PutFile {
            source,
            destination,
        } => bridge_file_transfer(context, true, source, destination),
        BridgeOperation::GetFile {
            source,
            destination,
        } => bridge_file_transfer(context, false, source, destination),
    }
}

#[cfg(unix)]
fn bridge_exec(
    context: &BridgeContext,
    argv: Vec<String>,
    cwd: String,
    env: Vec<String>,
    user: String,
) -> Result<serde_json::Value> {
    if argv.is_empty() {
        bail!("Ansible bridge exec request has no command arguments");
    }
    let mut arguments = vec![
        "exec".to_owned(),
        context.box_id.clone(),
        "--cwd".to_owned(),
        cwd,
        "--user".to_owned(),
        user,
    ];
    for entry in env {
        arguments.push("--env".to_owned());
        arguments.push(entry);
    }
    arguments.push("--".to_owned());
    arguments.extend(argv);
    let output = run_bridge_command(context, &arguments)?;
    let mut payload = match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        Ok(value) => value,
        Err(error) => {
            bail!(
                "pbox exec bridge returned invalid JSON: {}; {}",
                error,
                bridge_output_detail(context, &output)
            );
        }
    };
    let object = payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("pbox exec bridge returned a non-object JSON value"))?;
    object.insert(
        "protocol".to_owned(),
        serde_json::json!(BRIDGE_PROTOCOL_VERSION),
    );
    Ok(payload)
}

#[cfg(unix)]
fn bridge_file_transfer(
    context: &BridgeContext,
    upload: bool,
    source: String,
    destination: String,
) -> Result<serde_json::Value> {
    let remote_path = if upload {
        destination.clone()
    } else {
        source.clone()
    };
    let remote = format!("{}:{remote_path}", context.box_id);
    let arguments = if upload {
        vec!["scp".to_owned(), source, remote]
    } else {
        vec!["scp".to_owned(), remote, destination]
    };
    let output = run_bridge_command(context, &arguments)?;
    if !output.status.success() {
        bail!(
            "pbox file transfer failed: {}",
            bridge_output_detail(context, &output)
        );
    }
    Ok(serde_json::json!({
        "protocol": BRIDGE_PROTOCOL_VERSION,
        "ok": true,
    }))
}

#[cfg(unix)]
fn run_bridge_command(
    context: &BridgeContext,
    arguments: &[String],
) -> Result<std::process::Output> {
    let mut command = Command::new(&context.pbox_binary);
    command.env_clear();
    for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .arg("--config")
        .arg(&context.config_path)
        .arg("--color")
        .arg("never")
        .arg("--json")
        .args(arguments)
        .output()
        .context("run pbox Ansible bridge operation")
}

#[cfg(unix)]
fn bridge_output_detail(context: &BridgeContext, output: &std::process::Output) -> String {
    let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        detail = format!(
            "pbox exited with {}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "a signal".to_owned())
        );
    }
    detail.replace(context.config_path.to_string_lossy().as_ref(), "<config>")
}

fn run_ansible(invocation: &AnsibleInvocation<'_>, playbook: &Path, json: bool) -> Result<()> {
    #[cfg(unix)]
    {
        run_ansible_unix(invocation, playbook, json)
    }
    #[cfg(not(unix))]
    {
        let _ = (invocation, playbook, json);
        bail!("the pbox Ansible bridge requires a Unix-domain socket")
    }
}

#[cfg(unix)]
fn run_ansible_unix(invocation: &AnsibleInvocation<'_>, playbook: &Path, json: bool) -> Result<()> {
    let config_path = absolute_path(invocation.config_path)?;
    let pbox_binary = absolute_path(invocation.pbox_binary)?;
    let bridge = BridgeHandle::start(invocation, config_path, pbox_binary)?;
    let mut command = Command::new("ansible-playbook");
    command.env_clear();
    for key in ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .arg("--inventory")
        .arg(invocation.inventory)
        .arg("--connection")
        .arg(CONNECTION_PLUGIN_NAME)
        .arg("--limit")
        .arg(invocation.box_id)
        .arg(playbook)
        .current_dir(invocation.repository_root)
        .env("ANSIBLE_CONNECTION_PLUGINS", invocation.plugin_directory)
        .env(
            "ANSIBLE_ROLES_PATH",
            invocation.repository_root.join("roles"),
        )
        .env("ANSIBLE_HOST_KEY_CHECKING", "False")
        .env("PBOX_BRIDGE_SOCKET", bridge.socket_path())
        .env("PBOX_BOX_ID", invocation.box_id)
        .stdin(Stdio::inherit());

    let result = (|| -> Result<()> {
        let verbose = crate::progress::verbose();
        let stage = crate::ui::RecipeStage::start(
            if playbook
                .file_name()
                .is_some_and(|name| name == "preflight.yml")
            {
                "Preparing guest"
            } else {
                "Applying recipe"
            },
            !json && !verbose,
        );
        let log_path = invocation.operation_directory.with_extension("log");
        let log = fs::File::create(&log_path).context("create recipe log")?;
        set_mode(&log_path, 0o600)?;
        let error_log = log.try_clone().context("clone recipe log")?;
        let callbacks = invocation.operation_directory.join("callback_plugins");
        fs::create_dir_all(&callbacks)?;
        write_file(
            &callbacks.join("pbox_progress.py"),
            include_str!("ansible-plugins/pbox_progress.py"),
            0o600,
        )?;
        let events_path = invocation.operation_directory.join("events.jsonl");
        write_file(&events_path, "", 0o600)?;
        let events_file = fs::File::open(events_path.clone())?;
        command
            .env("ANSIBLE_CALLBACK_PLUGINS", callbacks)
            .env("ANSIBLE_CALLBACKS_ENABLED", "pbox_progress")
            .env("PBOX_EVENTS", events_path);
        command.env("ANSIBLE_NOCOLOR", "1");
        command.env("ANSIBLE_STDOUT_CALLBACK", "default");
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run ansible-playbook; install Ansible on the control machine")?;
        let stdout = child.stdout.take().context("missing Ansible stdout")?;
        let stderr = child.stderr.take().context("missing Ansible stderr")?;
        let events = stage.events();
        let stop = Arc::new(AtomicBool::new(false));
        let event_stop = Arc::clone(&stop);
        let events_thread = thread::spawn(move || read_events(events_file, event_stop, events));
        let stdout_thread = thread::spawn(move || capture_output(stdout, log, verbose));
        let stderr_thread = thread::spawn(move || capture_output(stderr, error_log, verbose));
        let status = child.wait().context("wait for ansible-playbook");
        stop.store(true, Ordering::Release);
        let reason = events_thread.join().ok().flatten();
        stdout_thread
            .join()
            .map_err(|_| anyhow!("Ansible stdout thread panicked"))??;
        stderr_thread
            .join()
            .map_err(|_| anyhow!("Ansible stderr thread panicked"))??;
        let status = status?;
        if !status.success() {
            return Err(AnsibleFailure {
                reason: reason.unwrap_or_else(|| format!("Ansible exited with {}", status)),
                log_path,
            }
            .into());
        }
        fs::remove_file(log_path).context("remove successful recipe log")?;
        stage.finish();
        Ok(())
    })();
    let bridge_result = bridge.stop();
    match (result, bridge_result) {
        (Err(error), Err(bridge_error)) => Err(error.context(bridge_error)),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(bridge_error)) => Err(bridge_error),
        (Ok(()), Ok(())) => Ok(()),
    }
}
#[derive(Debug)]
pub(crate) struct AnsibleFailure {
    pub(crate) reason: String,
    pub(crate) log_path: PathBuf,
}
impl std::fmt::Display for AnsibleFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for AnsibleFailure {}

#[derive(Deserialize)]
struct CallbackEvent {
    version: u32,
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    skipped: bool,
}
fn decode_event(line: &str) -> Option<CallbackEvent> {
    serde_json::from_str::<CallbackEvent>(line)
        .ok()
        .filter(|event| event.version == 1)
}

#[cfg(unix)]
fn read_events(
    mut file: fs::File,
    stop: Arc<AtomicBool>,
    sender: Option<std::sync::mpsc::Sender<crate::ui::RecipeEvent>>,
) -> Option<String> {
    use crate::ui::RecipeEvent;
    let mut pending = String::new();
    let mut reason = None;
    loop {
        let finished = stop.load(Ordering::Acquire);
        // Display telemetry is best effort. Unknown or malformed events cannot fail a recipe.
        if file.read_to_string(&mut pending).is_err() {
            break;
        }
        while let Some(end) = pending.find('\n') {
            let line: String = pending.drain(..=end).collect();
            if let Some(event) = decode_event(&line) {
                let rendered = match event.kind.as_str() {
                    "task" => Some(RecipeEvent::Task(event.text)),
                    "result" => Some(RecipeEvent::Result {
                        success: event.success,
                        skipped: event.skipped,
                    }),
                    "log" => Some(RecipeEvent::Log(event.text)),
                    "error" => {
                        reason = Some(event.text);
                        None
                    }
                    _ => None,
                };
                if let (Some(sender), Some(event)) = (&sender, rendered) {
                    let _ = sender.send(event);
                }
            }
        }
        if finished {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    reason
}

fn capture_output<R: Read>(mut reader: R, mut log: fs::File, verbose: bool) -> io::Result<()> {
    let mut bytes = [0; 8192];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        log.write_all(&bytes[..count])?;
        if verbose {
            crate::ui::recipe_raw_output(&bytes[..count])?;
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    Ok(std::env::current_dir()
        .context("read current directory")?
        .join(path))
}

fn role_name(repository_root: &Path, recipe_path: &str) -> Result<String> {
    role_directory(repository_root, Path::new(recipe_path))?
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("role recipe name is not UTF-8: {recipe_path}"))
}

fn role_directory(repository_root: &Path, path: &Path) -> Result<PathBuf> {
    let components = path.components().collect::<Vec<_>>();
    let is_role_directory = components.len() == 2
        && components[0].as_os_str() == "roles"
        && components[1].as_os_str() != "";
    let is_role_main = components.len() == 4
        && components[0].as_os_str() == "roles"
        && components[2].as_os_str() == "tasks"
        && matches!(
            components[3].as_os_str().to_str(),
            Some("main.yml" | "main.yaml")
        );
    if !is_role_directory && !is_role_main {
        bail!(
            "recipe path is not a supported role path: {}",
            path.display()
        );
    }
    let role = components[1].as_os_str();
    let role_directory = repository_root.join("roles").join(role);
    repository_path(
        repository_root,
        role_directory.strip_prefix(repository_root).unwrap(),
    )?;
    let mut has_main_task = false;
    for name in ["main.yml", "main.yaml"] {
        let task_path = role_directory.join("tasks").join(name);
        reject_symlink_ancestors(
            repository_root,
            task_path.strip_prefix(repository_root).unwrap(),
        )?;
        match fs::symlink_metadata(&task_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "recipe role task must not be a symbolic link: {}",
                    task_path.display()
                );
            }
            Ok(metadata) if metadata.file_type().is_file() => has_main_task = true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read recipe role task {}", task_path.display()));
            }
        }
    }
    if !has_main_task {
        bail!("recipe role has no tasks/main.yml: {}", path.display());
    }
    Ok(role_directory)
}

fn repository_path(repository_root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute() {
        bail!(
            "recipe path must be relative to the repository: {}",
            relative.display()
        );
    }
    let root = fs::canonicalize(repository_root)
        .with_context(|| format!("resolve recipe repository {}", repository_root.display()))?;
    let candidate = repository_root.join(relative);
    let resolved = fs::canonicalize(&candidate)
        .with_context(|| format!("resolve recipe path {}", candidate.display()))?;
    if !resolved.starts_with(&root) {
        bail!("recipe path escapes the repository: {}", relative.display());
    }
    Ok(candidate)
}

fn reject_symlink_ancestors(repository_root: &Path, relative: &Path) -> Result<()> {
    let mut current = repository_root.to_owned();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "recipe path must not contain a symbolic link: {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read recipe path {}", current.display()));
            }
        }
    }
    Ok(())
}

fn operation_directory(recipe: &str, box_id: &str) -> Result<PathBuf> {
    let state_root = dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pbox")
        .join("operations");
    fs::create_dir_all(&state_root).with_context(|| {
        format!(
            "create pbox operation state directory {}",
            state_root.display()
        )
    })?;
    set_mode(&state_root, 0o700)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("read system clock")?
        .as_nanos();
    let name = format!(
        "recipe-{}-{}-{}",
        safe_component(recipe),
        safe_component(box_id),
        stamp
    );
    let directory = state_root.join(name);
    fs::create_dir(&directory)
        .with_context(|| format!("create recipe operation directory {}", directory.display()))?;
    Ok(directory)
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn validate_box_id(value: &str) -> Result<()> {
    value
        .parse::<PboxId>()
        .map(|_| ())
        .map_err(|error| anyhow!("invalid pbox identifier {value}: {error}"))
}

fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    set_mode(path, mode)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("restrict {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn preflight_playbook() -> &'static str {
    r#"---
- name: Prepare the guest for Ansible modules
  hosts: all
  gather_facts: false
  tasks:
    - name: Ensure Python is available
      ansible.builtin.raw: >-
        if command -v python3 >/dev/null 2>&1; then exit 0; fi;
        if command -v apt-get >/dev/null 2>&1; then
        export DEBIAN_FRONTEND=noninteractive;
        apt-get update && apt-get install -y --no-install-recommends python3 || exit $?;
        elif command -v pacman >/dev/null 2>&1; then
        pacman -Syu --needed --noconfirm python || exit $?;
        elif command -v dnf >/dev/null 2>&1; then
        dnf install -y python3 || exit $?;
        elif command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive install python3 || exit $?;
        else echo 'Install python3 in this guest before applying recipes' >&2; exit 1; fi;
        echo PBOX_PYTHON_INSTALLED
      register: pbox_python
      changed_when: "'PBOX_PYTHON_INSTALLED' in pbox_python.stdout"
"#
}

fn role_playbook(role_name: &str) -> String {
    format!(
        "---\n- name: Apply pbox recipe role\n  hosts: all\n  gather_facts: true\n  roles:\n    - {}\n",
        serde_json::to_string(role_name).expect("role names are valid JSON strings")
    )
}

fn connection_plugin_source() -> &'static str {
    r#"from __future__ import absolute_import, division, print_function

__metaclass__ = type

import base64
import binascii
import json
import os
import socket

from ansible.errors import AnsibleError
from ansible.plugins.connection import ConnectionBase

BRIDGE_PROTOCOL_VERSION = 1
MAX_BRIDGE_RESPONSE_BYTES = 192 * 1024 * 1024

DOCUMENTATION = r'''
---
name: pbox_agent
short_description: Execute Ansible operations through pbox-agent
version_added: '0.1.0'
description:
  - Uses an ephemeral Rust-owned local bridge for authenticated pbox-agent operations.
  - The Ansible process never receives the PVE API token or its configuration path.
author:
  - pbox project
'''


class Connection(ConnectionBase):
    transport = 'pbox_agent'
    has_pipelining = False
    has_tty = False

    def _connect(self):
        self._connected = True
        return self

    def _target_box(self):
        target = self._play_context.remote_addr
        expected = os.environ.get('PBOX_BOX_ID')
        if expected and target != expected:
            raise AnsibleError(
                'delegated target %s does not match the selected pbox %s'
                % (target, expected)
            )
        return target

    def _bridge_request(self, kind, **values):
        socket_path = os.environ.get('PBOX_BRIDGE_SOCKET')
        if not socket_path:
            raise AnsibleError(
                'PBOX_BRIDGE_SOCKET is not configured for the pbox_agent connection'
            )
        payload = {
            'protocol': BRIDGE_PROTOCOL_VERSION,
            'kind': kind,
            'box_id': self._target_box(),
        }
        payload.update(values)
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                channel.settimeout(3600)
                channel.connect(socket_path)
                channel.sendall(
                    (
                        json.dumps(payload, separators=(',', ':')) + '\n'
                    ).encode('utf-8')
                )
                response = bytearray()
                while not response.endswith(b'\n'):
                    chunk = channel.recv(65536)
                    if not chunk:
                        break
                    response.extend(chunk)
                    if len(response) > MAX_BRIDGE_RESPONSE_BYTES:
                        raise AnsibleError('pbox bridge response exceeds the size limit')
        except AnsibleError:
            raise
        except OSError as error:
            raise AnsibleError('pbox bridge request failed: %s' % error)
        try:
            result = json.loads(bytes(response).decode('utf-8'))
        except (UnicodeDecodeError, ValueError) as error:
            raise AnsibleError('pbox bridge returned invalid JSON: %s' % error)
        if not isinstance(result, dict):
            raise AnsibleError('pbox bridge returned a non-object JSON value')
        if result.get('error'):
            raise AnsibleError('pbox bridge request failed: %s' % result['error'])
        return result

    def exec_command(self, cmd, in_data=None, sudoable=True):
        if in_data:
            raise AnsibleError('pbox_agent does not support pipelined stdin')
        payload = self._bridge_request(
            'exec',
            argv=['/bin/sh', '-c', cmd],
            cwd='/home/pbox',
            env=[],
            user=self._play_context.remote_user or 'root',
        )
        try:
            return (
                int(payload['exit_code']),
                base64.b64decode(payload['stdout_base64'], validate=True),
                base64.b64decode(payload['stderr_base64'], validate=True),
            )
        except (KeyError, TypeError, ValueError, binascii.Error) as error:
            raise AnsibleError('pbox exec returned an invalid JSON payload: %s' % error)

    def put_file(self, in_path, out_path):
        self._bridge_request(
            'put_file',
            source=in_path,
            destination=out_path,
        )

    def fetch_file(self, in_path, out_path):
        self._bridge_request(
            'get_file',
            source=in_path,
            destination=out_path,
        )

    def close(self):
        self._connected = False
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn callback_events_ignore_console_text_and_unknown_versions() {
        assert!(decode_event("TASK [whatever] ***").is_none());
        assert!(decode_event(r#"{"version":2,"kind":"task","text":"new"}"#).is_none());
        assert!(decode_event("broken JSON").is_none());
        let event =
            decode_event(r#"{"version":1,"kind":"task","text":"Install","future":true}"#).unwrap();
        assert_eq!(event.text, "Install");
    }

    #[test]
    fn recipe_capture_preserves_arbitrary_console_bytes() {
        let directory = operation_directory("output-test", "pbx_abcd1234").unwrap();
        let path = directory.join("output.log");
        let bytes = b"new format\xff\nTASK changed entirely\n";
        capture_output(&bytes[..], fs::File::create(&path).unwrap(), false).unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn plugin_source_contains_required_connection_methods() {
        let source = connection_plugin_source();
        assert!(source.contains("class Connection(ConnectionBase):"));
        assert!(source.contains("def _target_box"));
        assert!(source.contains("PBOX_BOX_ID"));
        assert!(source.contains("def exec_command"));
        assert!(source.contains("def put_file"));
        assert!(source.contains("def fetch_file"));
        assert!(source.contains("def close"));
        assert!(source.contains("PBOX_BRIDGE_SOCKET"));
        assert!(source.contains("BRIDGE_PROTOCOL_VERSION"));
        assert!(!source.contains("PBOX_CONFIG_FILE"));
        assert!(!source.contains("PBOX_BIN"));
        assert!(!source.contains("subprocess"));
    }

    #[cfg(unix)]
    #[test]
    fn bridge_rejects_wrong_protocol_and_box() {
        let context = BridgeContext {
            config_path: PathBuf::from("/tmp/pbox-config"),
            pbox_binary: PathBuf::from("/bin/false"),
            box_id: "pbx_abcd1234".to_owned(),
        };
        let wrong_protocol = BridgeRequest {
            protocol: BRIDGE_PROTOCOL_VERSION + 1,
            box_id: context.box_id.clone(),
            operation: BridgeOperation::Exec {
                argv: vec!["true".to_owned()],
                cwd: "/".to_owned(),
                env: Vec::new(),
                user: "root".to_owned(),
            },
        };
        assert!(handle_bridge_request(wrong_protocol, &context).is_err());

        let wrong_box = BridgeRequest {
            protocol: BRIDGE_PROTOCOL_VERSION,
            box_id: "pbx_other123".to_owned(),
            operation: BridgeOperation::Exec {
                argv: vec!["true".to_owned()],
                cwd: "/".to_owned(),
                env: Vec::new(),
                user: "root".to_owned(),
            },
        };
        assert!(handle_bridge_request(wrong_box, &context).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bridge_socket_round_trip_uses_rust_owned_request_boundary() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("pbox-ansible-bridge-test-{suffix}"));
        fs::create_dir_all(&root).expect("create bridge directory");
        let binary = root.join("pbox");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' '{\"exit_code\":0,\"stdout_base64\":\"\",\"stderr_base64\":\"\"}'\n",
        )
        .expect("write fake pbox");
        set_mode(&binary, 0o700).expect("make fake pbox executable");
        let invocation = AnsibleInvocation {
            config_path: Path::new("/tmp/pbox-config"),
            pbox_binary: &binary,
            repository_root: &root,
            operation_directory: &root,
            plugin_directory: &root,
            inventory: &root,
            box_id: "pbx_abcd1234",
        };
        let bridge = BridgeHandle::start(
            &invocation,
            PathBuf::from("/tmp/pbox-config"),
            binary.clone(),
        )
        .expect("start bridge");
        let mut stream = UnixStream::connect(bridge.socket_path()).expect("connect bridge");
        stream
            .write_all(
                br#"{"protocol":1,"kind":"exec","box_id":"pbx_abcd1234","argv":["true"],"cwd":"/","env":[],"user":"root"}
"#,
            )
            .expect("write request");
        let response = read_bridge_line(&mut stream).expect("read response");
        let response: serde_json::Value =
            serde_json::from_slice(&response).expect("decode response");
        assert_eq!(response["protocol"], BRIDGE_PROTOCOL_VERSION);
        assert_eq!(response["exit_code"], 0);
        drop(stream);
        bridge.stop().expect("stop bridge");
        fs::remove_dir_all(root).expect("remove bridge directory");
    }

    #[cfg(unix)]
    #[test]
    fn bridge_request_size_is_bounded() {
        let (mut writer, mut reader) = UnixStream::pair().expect("create socket pair");
        let writer_thread = thread::spawn(move || {
            writer
                .write_all(&vec![b'x'; MAX_BRIDGE_REQUEST_BYTES + 1])
                .expect("write oversized request");
        });
        let error = read_bridge_line(&mut reader).expect_err("reject oversized request");
        writer_thread.join().expect("join oversized request writer");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn operation_components_are_filesystem_safe() {
        let directory =
            operation_directory("desktop/xfce", "pbx_abcd1234").expect("operation directory");
        assert!(
            directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("desktop_xfce")
        );
        fs::remove_dir_all(directory).expect("remove operation directory");
    }

    #[test]
    fn role_paths_resolve_to_role_names() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("pbox-ansible-role-test-{suffix}"));
        fs::create_dir_all(root.join("roles/demo/tasks")).expect("create role directory");
        fs::write(
            root.join("roles/demo/tasks/main.yml"),
            "---\n- hosts: all\n",
        )
        .expect("write role task");

        assert_eq!(
            role_name(&root, "roles/demo/tasks/main.yml").expect("resolve role"),
            "demo"
        );
        fs::remove_dir_all(root).expect("remove role fixture");
    }
}
