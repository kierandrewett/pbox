use crate::recipes::{Recipe, RecipeCatalog, RecipeKind};
use anyhow::{Context, Result, anyhow, bail};
use pbox_core::PboxId;
use serde::Serialize;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const CONNECTION_PLUGIN_NAME: &str = "pbox_agent";

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
    plugin_directory: &'a Path,
    inventory: &'a Path,
    box_id: &'a str,
}

pub fn apply_recipe(
    config_path: &Path,
    pbox_binary: &Path,
    repository_root: &Path,
    catalog: &RecipeCatalog,
    recipe: &Recipe,
    box_id: &str,
    json: bool,
) -> Result<RecipeApplyResult> {
    validate_box_id(box_id)?;
    if !repository_root.is_dir() {
        bail!(
            "recipe repository directory does not exist: {}",
            repository_root.display()
        );
    }
    let source_path = repository_path(repository_root, Path::new(&recipe.path))?;
    let operation_directory = operation_directory(&recipe.id, box_id)?;
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
            plugin_directory: &plugin_directory,
            inventory: &inventory,
            box_id,
        };
        run_ansible(&invocation, &preflight, json)
            .context("ensure the guest has a Python interpreter for Ansible")?;

        let playbook = match recipe.kind {
            RecipeKind::Playbook => source_path,
            RecipeKind::Role => {
                let role_name = role_name(repository_root, &recipe.path)?;
                let wrapper = operation_directory.join("recipe.yml");
                write_file(&wrapper, &role_playbook(&role_name), 0o600)?;
                wrapper
            }
        };
        run_ansible(&invocation, &playbook, json)
            .with_context(|| format!("apply recipe {} to box {box_id}", recipe.id))?;
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
                recipe: recipe.id.clone(),
                box_id: box_id.to_owned(),
                repository: catalog.repository.clone(),
                revision: catalog.revision.clone(),
            },
            cleanup_error: cleanup_result.err(),
        }),
    }
}

fn run_ansible(invocation: &AnsibleInvocation<'_>, playbook: &Path, json: bool) -> Result<()> {
    let config_path = absolute_path(invocation.config_path)?;
    let pbox_binary = absolute_path(invocation.pbox_binary)?;
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
        .env("PBOX_BIN", pbox_binary)
        .env("PBOX_CONFIG_FILE", config_path)
        .env("PBOX_BOX_ID", invocation.box_id)
        .stdin(Stdio::inherit());

    if json {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run ansible-playbook; install Ansible on the control machine")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ansible-playbook stdout pipe was not created"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("ansible-playbook stderr pipe was not created"))?;
        let stdout_thread = thread::spawn(|| stream_child_output(stdout));
        let stderr_thread = thread::spawn(|| stream_child_output(stderr));
        let status = child.wait().context("wait for ansible-playbook")?;
        stdout_thread
            .join()
            .map_err(|_| anyhow!("Ansible stdout stream thread panicked"))??;
        stderr_thread
            .join()
            .map_err(|_| anyhow!("Ansible stderr stream thread panicked"))??;
        ensure_success(status, "ansible-playbook")
    } else {
        let status = command
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("run ansible-playbook; install Ansible on the control machine")?;
        ensure_success(status, "ansible-playbook")
    }
}
fn stream_child_output<R: Read>(mut reader: R) -> std::io::Result<()> {
    let stderr = io::stderr();
    let mut target = stderr.lock();
    io::copy(&mut reader, &mut target).map(|_| ())
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

fn ensure_success(status: ExitStatus, command: &str) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    bail!(
        "{command} exited with {}",
        status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "a signal".to_owned())
    );
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
    - name: Install Python on Debian guests when it is absent
      ansible.builtin.raw: >-
        if command -v python3 >/dev/null 2>&1; then exit 0; fi;
        if ! command -v apt-get >/dev/null 2>&1; then
        echo 'python3 is missing and apt-get is unavailable' >&2; exit 1; fi;
        export DEBIAN_FRONTEND=noninteractive;
        apt-get update && apt-get install -y --no-install-recommends python3
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
import subprocess

from ansible.errors import AnsibleError
from ansible.plugins.connection import ConnectionBase

DOCUMENTATION = r'''
---
name: pbox_agent
short_description: Execute Ansible operations through pbox-agent
version_added: '0.1.0'
description:
  - Uses the pbox CLI as the authenticated control-plane client.
  - The guest never receives the PVE API token or its derived trust material.
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

    def _pbox_command(self, arguments):
        binary = os.environ.get('PBOX_BIN')
        if not binary:
            raise AnsibleError('PBOX_BIN is not configured for the pbox_agent connection')
        command = [binary]
        config = os.environ.get('PBOX_CONFIG_FILE')
        if config:
            command.extend(['--config', config])
        command.extend(['--color', 'never', '--json'])
        command.extend(arguments)
        return command

    def _run(self, arguments, input_data=None):
        try:
            result = subprocess.run(
                self._pbox_command(arguments),
                input=input_data,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        except OSError as error:
            raise AnsibleError('cannot start pbox: %s' % error)
        try:
            payload = json.loads(result.stdout.decode('utf-8'))
        except (UnicodeDecodeError, ValueError) as error:
            detail = result.stderr.decode('utf-8', errors='replace').strip()
            raise AnsibleError('pbox command failed: %s' % (detail or error))
        if not isinstance(payload, dict):
            raise AnsibleError('pbox command returned a non-object JSON value')
        return payload

    def _run_file_transfer(self, arguments):
        try:
            return subprocess.run(
                self._pbox_command(arguments),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        except OSError as error:
            raise AnsibleError('cannot start pbox: %s' % error)

    def exec_command(self, cmd, in_data=None, sudoable=True):
        if in_data:
            raise AnsibleError('pbox_agent does not support pipelined stdin')
        payload = self._run([
            'exec',
            self._target_box(),
            '--user',
            self._play_context.remote_user or 'root',
            '--',
            '/bin/sh',
            '-c',
            cmd,
        ])
        try:
            return (
                int(payload['exit_code']),
                base64.b64decode(payload['stdout_base64'], validate=True),
                base64.b64decode(payload['stderr_base64'], validate=True),
            )
        except (KeyError, TypeError, ValueError, binascii.Error) as error:
            raise AnsibleError('pbox exec returned an invalid JSON payload: %s' % error)

    def put_file(self, in_path, out_path):
        result = self._run_file_transfer([
            'scp',
            in_path,
            '%s:%s' % (self._target_box(), out_path),
        ])
        if result.returncode != 0:
            raise AnsibleError(result.stderr.decode('utf-8', errors='replace').strip() or 'pbox upload failed')

    def fetch_file(self, in_path, out_path):
        result = self._run_file_transfer([
            'scp',
            '%s:%s' % (self._target_box(), in_path),
            out_path,
        ])
        if result.returncode != 0:
            raise AnsibleError(result.stderr.decode('utf-8', errors='replace').strip() or 'pbox download failed')

    def close(self):
        self._connected = False
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

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
