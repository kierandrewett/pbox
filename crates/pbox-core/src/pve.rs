use crate::Secret;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;
use thiserror::Error;
const API_PREFIX: &str = "/api2/json";
const PVE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub trait PveApi {
    fn list_cluster_resources(&self) -> Result<Vec<ClusterResource>, PveError>;
    fn list_nodes(&self) -> Result<Vec<PveNode>, PveError>;
    fn list_node_storages(&self, node: &str) -> Result<Vec<PveStorage>, PveError>;
    fn list_storage_content(
        &self,
        node: &str,
        storage: &str,
        content: &str,
    ) -> Result<Vec<PveStorageContent>, PveError>;
    fn get_lxc_config(&self, node: &str, vmid: u64) -> Result<LxcConfig, PveError>;
    fn list_lxc_interfaces(&self, node: &str, vmid: u64) -> Result<Vec<LxcInterface>, PveError>;
    fn list_lxc_snapshots(&self, node: &str, vmid: u64) -> Result<Vec<LxcSnapshot>, PveError>;
    fn get_task_status(&self, node: &str, upid: &str) -> Result<PveTaskStatus, PveError>;
    fn create_lxc(
        &self,
        node: &str,
        vmid: u64,
        request: &LxcCreateRequest,
    ) -> Result<PveTaskResponse, PveError>;
    fn update_lxc_config(
        &self,
        node: &str,
        vmid: u64,
        request: &LxcConfigUpdateRequest,
    ) -> Result<(), PveError>;
    fn start_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn shutdown_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn stop_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn delete_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn create_lxc_snapshot(
        &self,
        node: &str,
        vmid: u64,
        request: &LxcSnapshotRequest,
    ) -> Result<PveTaskResponse, PveError>;
    fn rollback_lxc_snapshot(
        &self,
        node: &str,
        vmid: u64,
        snapname: &str,
        start: bool,
    ) -> Result<PveTaskResponse, PveError>;
    fn delete_lxc_snapshot(
        &self,
        node: &str,
        vmid: u64,
        snapname: &str,
    ) -> Result<PveTaskResponse, PveError>;
}

#[derive(Clone)]
pub struct PveClientConfig {
    pub base_url: String,
    pub token_id: String,
    pub token_secret: Secret,
    pub tls_insecure: bool,
}

impl fmt::Debug for PveClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PveClientConfig")
            .field("base_url", &self.base_url)
            .field("token_id", &self.token_id)
            .field("token_secret", &"<redacted>")
            .field("tls_insecure", &self.tls_insecure)
            .finish()
    }
}

impl PveClientConfig {
    pub fn new(
        base_url: impl Into<String>,
        token_id: impl Into<String>,
        token_secret: Secret,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            token_id: token_id.into(),
            token_secret,
            tls_insecure: false,
        }
    }
}

pub struct PveClient {
    http: Client,
    base_url: String,
    authorization: String,
}

impl fmt::Debug for PveClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PveClient")
            .field("base_url", &self.base_url)
            .field("authorization", &"<redacted>")
            .finish()
    }
}
impl PveClient {
    pub fn new(config: PveClientConfig) -> Result<Self, PveError> {
        let http = Client::builder()
            .connect_timeout(PVE_CONNECT_TIMEOUT)
            .timeout(PVE_REQUEST_TIMEOUT)
            .danger_accept_invalid_certs(config.tls_insecure)
            .build()
            .map_err(PveError::Client)?;
        let base_url = normalise_base_url(&config.base_url)?;
        let authorization = format!(
            "PVEAPIToken={}= {}",
            config.token_id,
            config.token_secret.expose()
        )
        .replace("= ", "=");
        Ok(Self {
            http,
            base_url,
            authorization,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
            .header("Authorization", &self.authorization)
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, PveError> {
        let response = self
            .request(Method::GET, path)
            .send()
            .map_err(PveError::Request)?;
        decode_response(response)
    }

    fn task<T: Serialize>(
        &self,
        method: Method,
        path: &str,
        form: &T,
    ) -> Result<PveTaskResponse, PveError> {
        let response = self
            .request(method, path)
            .form(form)
            .send()
            .map_err(PveError::Request)?;
        decode_task_response(response)
    }
    fn empty<T: Serialize>(&self, method: Method, path: &str, form: &T) -> Result<(), PveError> {
        let response = self
            .request(method, path)
            .form(form)
            .send()
            .map_err(PveError::Request)?;
        let _: serde_json::Value = decode_response(response)?;
        Ok(())
    }

    fn task_without_form(&self, method: Method, path: &str) -> Result<PveTaskResponse, PveError> {
        let response = self
            .request(method, path)
            .send()
            .map_err(PveError::Request)?;
        decode_task_response(response)
    }
}

impl PveApi for PveClient {
    fn list_cluster_resources(&self) -> Result<Vec<ClusterResource>, PveError> {
        self.get("/cluster/resources?type=vm")
    }

    fn get_lxc_config(&self, node: &str, vmid: u64) -> Result<LxcConfig, PveError> {
        validate_path_segment(node, "node")?;
        self.get(&format!("/nodes/{node}/lxc/{vmid}/config?current=1"))
    }

    fn list_lxc_interfaces(&self, node: &str, vmid: u64) -> Result<Vec<LxcInterface>, PveError> {
        validate_path_segment(node, "node")?;
        self.get(&format!("/nodes/{node}/lxc/{vmid}/interfaces"))
    }

    fn list_lxc_snapshots(&self, node: &str, vmid: u64) -> Result<Vec<LxcSnapshot>, PveError> {
        validate_path_segment(node, "node")?;
        self.get(&format!("/nodes/{node}/lxc/{vmid}/snapshot"))
    }

    fn get_task_status(&self, node: &str, upid: &str) -> Result<PveTaskStatus, PveError> {
        validate_path_segment(node, "node")?;
        validate_path_segment(upid, "UPID")?;
        self.get(&format!("/nodes/{node}/tasks/{upid}/status"))
    }

    fn list_nodes(&self) -> Result<Vec<PveNode>, PveError> {
        self.get("/nodes")
    }

    fn list_node_storages(&self, node: &str) -> Result<Vec<PveStorage>, PveError> {
        validate_path_segment(node, "node")?;
        self.get(&format!("/nodes/{node}/storage"))
    }

    fn list_storage_content(
        &self,
        node: &str,
        storage: &str,
        content: &str,
    ) -> Result<Vec<PveStorageContent>, PveError> {
        validate_path_segment(node, "node")?;
        validate_path_segment(storage, "storage")?;
        validate_path_segment(content, "content")?;
        self.get(&format!(
            "/nodes/{node}/storage/{storage}/content?content={content}"
        ))
    }

    fn create_lxc(
        &self,
        node: &str,
        vmid: u64,
        request: &LxcCreateRequest,
    ) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        let form = LxcCreateForm { vmid, request };
        self.task(Method::POST, &format!("/nodes/{node}/lxc"), &form)
    }

    fn update_lxc_config(
        &self,
        node: &str,
        vmid: u64,
        request: &LxcConfigUpdateRequest,
    ) -> Result<(), PveError> {
        validate_path_segment(node, "node")?;
        self.empty(
            Method::PUT,
            &format!("/nodes/{node}/lxc/{vmid}/config"),
            request,
        )
    }

    fn start_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError> {
        self.lxc_status_action(node, vmid, "start")
    }

    fn shutdown_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError> {
        self.lxc_status_action(node, vmid, "shutdown")
    }

    fn stop_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError> {
        self.lxc_status_action(node, vmid, "stop")
    }

    fn delete_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        self.task_without_form(Method::DELETE, &format!("/nodes/{node}/lxc/{vmid}"))
    }

    fn create_lxc_snapshot(
        &self,
        node: &str,
        vmid: u64,
        request: &LxcSnapshotRequest,
    ) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        validate_snapshot_name(&request.snapname)?;
        self.task(
            Method::POST,
            &format!("/nodes/{node}/lxc/{vmid}/snapshot"),
            request,
        )
    }

    fn rollback_lxc_snapshot(
        &self,
        node: &str,
        vmid: u64,
        snapname: &str,
        start: bool,
    ) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        validate_snapshot_name(snapname)?;
        let form = LxcSnapshotRollbackForm {
            start: u8::from(start),
        };
        self.task(
            Method::POST,
            &format!("/nodes/{node}/lxc/{vmid}/snapshot/{snapname}/rollback"),
            &form,
        )
    }

    fn delete_lxc_snapshot(
        &self,
        node: &str,
        vmid: u64,
        snapname: &str,
    ) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        validate_snapshot_name(snapname)?;
        self.task_without_form(
            Method::DELETE,
            &format!("/nodes/{node}/lxc/{vmid}/snapshot/{snapname}"),
        )
    }
}

impl PveClient {
    fn lxc_status_action(
        &self,
        node: &str,
        vmid: u64,
        action: &str,
    ) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        validate_path_segment(action, "action")?;
        self.task_without_form(
            Method::POST,
            &format!("/nodes/{node}/lxc/{vmid}/status/{action}"),
        )
    }
}

fn normalise_base_url(value: &str) -> Result<String, PveError> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(PveError::InvalidBaseUrl);
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| PveError::InvalidBaseUrl)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(PveError::InvalidBaseUrl);
    }
    if trimmed.ends_with(API_PREFIX) {
        Ok(trimmed.to_owned())
    } else {
        Ok(format!("{trimmed}{API_PREFIX}"))
    }
}

fn validate_path_segment(value: &str, field: &str) -> Result<(), PveError> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '?' | '#' | '%'))
    {
        return Err(PveError::InvalidPathSegment {
            field: field.to_owned(),
        });
    }
    Ok(())
}

fn validate_snapshot_name(value: &str) -> Result<(), PveError> {
    let valid = value.len() >= 2
        && !value.is_empty()
        && value.len() <= 40
        && value != "current"
        && value != "vzdump"
        && value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
        && value
            .chars()
            .skip(1)
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if !valid {
        return Err(PveError::InvalidSnapshotName {
            name: value.to_owned(),
        });
    }
    Ok(())
}

fn decode_response<T: DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> Result<T, PveError> {
    let status = response.status();
    let body = response.text().map_err(PveError::Request)?;
    if !status.is_success() {
        let message = serde_json::from_str::<PveErrorEnvelope>(&body)
            .ok()
            .and_then(|envelope| envelope.errors)
            .unwrap_or(body);
        return Err(PveError::Http { status, message });
    }
    let envelope: PveResponse<T> = serde_json::from_str(&body).map_err(PveError::Decode)?;
    Ok(envelope.data)
}

fn decode_task_response(
    response: reqwest::blocking::Response,
) -> Result<PveTaskResponse, PveError> {
    decode_response(response)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PveNode {
    pub node: String,
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PveStorage {
    pub storage: String,
    pub content: Option<String>,
    pub active: Option<u64>,
    pub enabled: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PveStorageContent {
    pub volid: String,
    pub content: Option<String>,
    pub format: Option<String>,
    #[serde(rename = "isBase")]
    pub is_base: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]

pub struct ClusterResource {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub vmid: Option<u64>,
    pub node: Option<String>,
    pub status: Option<String>,
    pub name: Option<String>,
    pub tags: Option<String>,
    pub uptime: Option<u64>,
    pub mem: Option<u64>,
    pub maxmem: Option<u64>,
    pub disk: Option<u64>,
    pub maxdisk: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LxcCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ostemplate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cores: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net0: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unprivileged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onboot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "ssh-public-keys", skip_serializing_if = "Option::is_none")]
    pub ssh_public_keys: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LxcConfigUpdateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cores: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net0: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unprivileged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LxcSnapshotRequest {
    pub snapname: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Snapshot data returned by the PVE LXC snapshot endpoint.
///
/// PVE includes a synthetic `current` entry and may add fields such as
/// `digest`, `running`, or `snapstate`. Keep those fields in `extra` so the
/// client remains compatible with PVE response additions.
/// Source: https://github.com/proxmox/pve-container/blob/master/src/PVE/API2/LXC/Snapshot.pm
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LxcSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub snaptime: Option<u64>,
    pub parent: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct LxcSnapshotRollbackForm {
    start: u8,
}

#[derive(Debug, Serialize)]
struct LxcCreateForm<'a> {
    vmid: u64,
    #[serde(flatten)]
    request: &'a LxcCreateRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LxcConfig {
    pub digest: Option<String>,
    pub description: Option<String>,
    pub hostname: Option<String>,
    pub cores: Option<u64>,
    pub memory: Option<u64>,
    pub swap: Option<u64>,
    pub rootfs: Option<String>,
    pub unprivileged: Option<bool>,
    pub net0: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Runtime interface data returned by the PVE LXC interfaces endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LxcInterface {
    pub name: Option<String>,
    pub hwaddr: Option<String>,
    pub inet: Option<String>,
    pub inet6: Option<String>,
    pub address: Option<String>,
    pub netmask: Option<String>,
    pub gateway: Option<String>,
    pub gateway6: Option<String>,
    pub method: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub exists: Option<u64>,
    pub active: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Select a reachable-looking IPv4 address from runtime LXC interfaces.
pub fn select_lxc_ipv4(interfaces: &[LxcInterface]) -> Option<Ipv4Addr> {
    interfaces
        .iter()
        .filter_map(|interface| {
            if interface.active == Some(0) || interface.exists == Some(0) {
                return None;
            }
            let address = interface.inet.as_deref()?.split('/').next()?;
            let address = address.parse::<Ipv4Addr>().ok()?;
            if address.is_unspecified() || address.is_loopback() || address.is_link_local() {
                return None;
            }
            Some((
                interface.name.as_deref() != Some("eth0"),
                address.octets(),
                address,
            ))
        })
        .min_by_key(|(not_eth0, octets, _)| (*not_eth0, *octets))
        .map(|(_, _, address)| address)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PveTaskResponse {
    pub upid: String,
}

impl<'de> Deserialize<'de> for PveTaskResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Raw(String),
            Object { upid: String },
        }

        match Wire::deserialize(deserializer)? {
            Wire::Raw(upid) | Wire::Object { upid } => Ok(Self { upid }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PveTaskStatus {
    pub status: String,
    pub exitstatus: Option<String>,
    pub upid: Option<String>,
    pub node: Option<String>,
    pub pid: Option<u64>,
    pub starttime: Option<u64>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

impl PveTaskStatus {
    pub fn is_successful(&self) -> bool {
        self.status == "stopped" && self.exitstatus.as_deref() == Some("OK")
    }
}

#[derive(Debug, Deserialize)]
struct PveResponse<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct PveErrorEnvelope {
    errors: Option<String>,
}

#[derive(Debug, Error)]
pub enum PveError {
    #[error("could not build PVE HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("could not reach PVE API: {0}")]
    Request(#[source] reqwest::Error),
    #[error("invalid PVE API response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("PVE API returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("PVE URL is empty")]
    InvalidBaseUrl,
    #[error("invalid {field} path segment")]
    InvalidPathSegment { field: String },
    #[error("invalid PVE snapshot name: {name}")]
    InvalidSnapshotName { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_token_secret() {
        let config = PveClientConfig::new(
            "https://pve.example.test:8006",
            "pbox@pve!cli",
            Secret::new("secret"),
        );
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(": \"secret\""));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn base_url_is_normalised_once() {
        assert_eq!(
            normalise_base_url("https://pve.test:8006/").unwrap(),
            "https://pve.test:8006/api2/json"
        );
        assert_eq!(
            normalise_base_url("https://pve.test:8006/api2/json").unwrap(),
            "https://pve.test:8006/api2/json"
        );
    }

    #[test]
    fn base_url_rejects_plain_http() {
        assert!(normalise_base_url("http://pve.test:8006").is_err());
    }
    #[test]
    fn base_url_rejects_credentials_and_query_data() {
        assert!(normalise_base_url("https://user:secret@pve.test:8006").is_err());
        assert!(normalise_base_url("https://pve.test:8006/?token=secret").is_err());
    }

    #[test]
    fn path_segments_cannot_escape_endpoint() {
        assert!(validate_path_segment("..", "node").is_err());
        assert!(validate_path_segment(".", "node").is_err());
        assert!(validate_path_segment(r"node\\child", "node").is_err());
        assert!(validate_path_segment("%2e%2e", "node").is_err());
        assert!(validate_path_segment("UPID:pve:1:2:3", "UPID").is_ok());
    }

    #[test]
    fn raw_upid_response_decodes() {
        let response: PveResponse<PveTaskResponse> =
            serde_json::from_str(r#"{"data":"UPID:pve:1:2:3:create"}"#).unwrap();
        assert_eq!(response.data.upid, "UPID:pve:1:2:3:create");
    }

    #[test]
    fn null_response_decodes_for_config_updates() {
        let response: PveResponse<serde_json::Value> =
            serde_json::from_str(r#"{"data":null}"#).unwrap();
        assert!(response.data.is_null());
    }

    #[test]
    fn provisioning_discovery_responses_decode() {
        let nodes: Vec<PveNode> =
            serde_json::from_str(r#"[{"node":"pve-a","status":"online","cpu":0.1}]"#).unwrap();
        assert_eq!(nodes[0].node, "pve-a");
        assert_eq!(nodes[0].status.as_deref(), Some("online"));
        assert_eq!(nodes[0].extra["cpu"], 0.1);

        let storages: Vec<PveStorage> = serde_json::from_str(
            r#"[{"storage":"local","content":"iso,vztmpl,rootdir","active":1,"enabled":1}]"#,
        )
        .unwrap();
        assert_eq!(storages[0].storage, "local");
        assert!(storages[0].content.as_deref().unwrap().contains("vztmpl"));

        let content: Vec<PveStorageContent> = serde_json::from_str(
            r#"[{"volid":"local:vztmpl/debian-13-standard_13.0-1_amd64.tar.zst","content":"vztmpl","format":"tar.zst","isBase":1}]"#,
        )
        .unwrap();
        assert_eq!(
            content[0].volid,
            "local:vztmpl/debian-13-standard_13.0-1_amd64.tar.zst"
        );
        assert_eq!(content[0].is_base, Some(1));
    }

    #[test]
    fn lxc_interfaces_decode_runtime_addresses_and_extra_fields() {
        let interfaces: Vec<LxcInterface> = serde_json::from_str(
            r#"[{"name":"eth0","hwaddr":"02:00:00:00:00:01","inet":"10.0.20.43/24","inet6":"fe80::1/64","type":"eth","active":1,"unexpected":"kept"}]"#,
        )
        .unwrap();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].name.as_deref(), Some("eth0"));
        assert_eq!(interfaces[0].inet.as_deref(), Some("10.0.20.43/24"));
        assert_eq!(interfaces[0].type_.as_deref(), Some("eth"));
        assert_eq!(interfaces[0].active, Some(1));
        assert_eq!(interfaces[0].extra["unexpected"], "kept");
    }

    #[test]
    fn lxc_snapshots_decode_current_and_stored_entries() {
        let snapshots: Vec<LxcSnapshot> = serde_json::from_str(
            r#"[{"name":"before_recipe","description":"Before recipe","snaptime":1724520000,"parent":"base","snapstate":"prepare"},{"name":"current","description":"You are here!","running":1,"digest":"deadbeef"}]"#,
        )
        .unwrap();
        assert_eq!(snapshots[0].name, "before_recipe");
        assert_eq!(snapshots[0].snaptime, Some(1_724_520_000));
        assert_eq!(snapshots[0].parent.as_deref(), Some("base"));
        assert_eq!(snapshots[0].extra["snapstate"], "prepare");
        assert_eq!(snapshots[1].name, "current");
        assert_eq!(snapshots[1].snaptime, None);
        assert_eq!(snapshots[1].extra["running"], 1);
    }

    #[test]
    fn lxc_snapshot_response_preserves_optional_fields() {
        let snapshot: LxcSnapshot =
            serde_json::from_str(r#"{"name":"checkpoint","description":""}"#).unwrap();
        assert_eq!(snapshot.description.as_deref(), Some(""));
        assert_eq!(snapshot.snaptime, None);
        assert_eq!(snapshot.parent, None);
    }

    #[test]
    fn task_status_maps_type_and_success() {
        let status: PveTaskStatus =
            serde_json::from_str(r#"{"status":"stopped","exitstatus":"OK","type":"vzcreate"}"#)
                .unwrap();
        assert_eq!(status.type_.as_deref(), Some("vzcreate"));
        assert!(status.is_successful());
    }

    #[test]
    fn task_status_failure_is_not_successful() {
        let status = PveTaskStatus {
            status: "stopped".to_owned(),
            exitstatus: Some("ERROR: create failed".to_owned()),
            upid: None,
            node: None,
            pid: None,
            starttime: None,
            type_: None,
        };
        assert!(!status.is_successful());
    }

    #[test]
    fn lifecycle_requests_serialize_only_set_values() {
        let request = LxcCreateRequest {
            ostemplate: Some("local:vztmpl/debian-12.tar.zst".to_owned()),
            memory: Some(1024),
            net0: Some("name=eth0,bridge=vmbr0".to_owned()),
            onboot: Some(true),
            ssh_public_keys: Some("ssh-ed25519 AAAA bootstrap".to_owned()),
            start: Some(true),
            ..Default::default()
        };
        let form = LxcCreateForm {
            vmid: 100,
            request: &request,
        };
        let value = serde_json::to_value(form).unwrap();
        assert_eq!(value["vmid"], 100);
        assert_eq!(value["ostemplate"], "local:vztmpl/debian-12.tar.zst");
        assert_eq!(value["memory"], 1024);
        assert_eq!(value["net0"], "name=eth0,bridge=vmbr0");
        assert_eq!(value["ssh-public-keys"], "ssh-ed25519 AAAA bootstrap");
        assert_eq!(value["onboot"], true);
        assert_eq!(value["start"], true);
        assert!(value.get("hostname").is_none());

        let update = LxcConfigUpdateRequest {
            digest: Some("deadbeef".to_owned()),
            description: Some("managed by pbox".to_owned()),
            ..Default::default()
        };
        let update_value = serde_json::to_value(update).unwrap();
        assert_eq!(update_value["digest"], "deadbeef");
        assert_eq!(update_value["description"], "managed by pbox");
        assert!(update_value.get("memory").is_none());
    }

    #[test]
    fn snapshot_requests_follow_pve_forms() {
        let request = LxcSnapshotRequest {
            snapname: "before_recipe".to_owned(),
            description: Some("Before applying recipe desktop/xfce".to_owned()),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["snapname"], "before_recipe");
        assert_eq!(value["description"], "Before applying recipe desktop/xfce");

        let rollback = serde_json::to_value(LxcSnapshotRollbackForm { start: 1 }).unwrap();
        assert_eq!(rollback["start"], 1);
    }

    #[test]
    fn snapshot_names_follow_proxmox_config_id_rules() {
        assert!(validate_snapshot_name("before_recipe").is_ok());
        assert!(validate_snapshot_name("A1-test").is_ok());
        for invalid in [
            "",
            "a",
            "1-before",
            "before recipe",
            "before.recipe",
            "current",
            "vzdump",
        ] {
            assert!(validate_snapshot_name(invalid).is_err(), "{invalid}");
        }
        assert!(validate_snapshot_name(&"a".repeat(41)).is_err());
    }
    #[test]
    fn lxc_ipv4_selection_prefers_eth0_and_skips_unusable_addresses() {
        let interfaces: Vec<LxcInterface> = serde_json::from_str(
            r#"[{"name":"lo","inet":"127.0.0.1/8"},{"name":"eth1","inet":"192.168.1.20/24"},{"name":"eth0","inet":"10.0.20.43/24"},{"name":"eth2","inet":"10.0.20.2/24","active":0}]"#,
        )
        .unwrap();
        assert_eq!(
            select_lxc_ipv4(&interfaces),
            Some("10.0.20.43".parse().unwrap())
        );
    }

    #[test]
    fn lxc_ipv4_selection_returns_none_without_active_addresses() {
        let interfaces: Vec<LxcInterface> =
            serde_json::from_str(r#"[{"name":"eth0","inet":"10.0.20.43/24","active":0}]"#).unwrap();
        assert_eq!(select_lxc_ipv4(&interfaces), None);
    }
}
