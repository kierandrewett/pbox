use crate::Secret;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

const API_PREFIX: &str = "/api2/json";

pub trait PveApi {
    fn list_cluster_resources(&self) -> Result<Vec<ClusterResource>, PveError>;
    fn get_lxc_config(&self, node: &str, vmid: u64) -> Result<LxcConfig, PveError>;
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
    ) -> Result<PveTaskResponse, PveError>;
    fn start_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn shutdown_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn stop_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
    fn delete_lxc(&self, node: &str, vmid: u64) -> Result<PveTaskResponse, PveError>;
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

    fn task<T: Serialize>(&self, method: Method, path: &str, form: &T) -> Result<PveTaskResponse, PveError> {
        let response = self
            .request(method, path)
            .form(form)
            .send()
            .map_err(PveError::Request)?;
        decode_task_response(response)
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

    fn get_task_status(&self, node: &str, upid: &str) -> Result<PveTaskStatus, PveError> {
        validate_path_segment(node, "node")?;
        validate_path_segment(upid, "UPID")?;
        self.get(&format!("/nodes/{node}/tasks/{upid}/status"))
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
    ) -> Result<PveTaskResponse, PveError> {
        validate_path_segment(node, "node")?;
        self.task(
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
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(PveError::InvalidBaseUrl);
    }
    if trimmed.ends_with(API_PREFIX) {
        Ok(trimmed.to_owned())
    } else {
        Ok(format!("{trimmed}{API_PREFIX}"))
    }
}

fn validate_path_segment(value: &str, field: &str) -> Result<(), PveError> {
    if value.is_empty() || value.contains('/') || value.contains('?') || value.contains('#') {
        return Err(PveError::InvalidPathSegment {
            field: field.to_owned(),
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
    pub description: Option<String>,
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
    fn path_segments_cannot_escape_endpoint() {
        assert!(validate_path_segment("node/a", "node").is_err());
        assert!(validate_path_segment("UPID:pve:1:2:3", "UPID").is_ok());
    }

    #[test]
    fn raw_upid_response_decodes() {
        let response: PveResponse<PveTaskResponse> =
            serde_json::from_str(r#"{"data":"UPID:pve:1:2:3:create"}"#).unwrap();
        assert_eq!(response.data.upid, "UPID:pve:1:2:3:create");
    }

    #[test]
    fn task_status_maps_type_and_success() {
        let status: PveTaskStatus = serde_json::from_str(
            r#"{"status":"stopped","exitstatus":"OK","type":"vzcreate"}"#,
        )
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
            start: Some(true),
            ..Default::default()
        };
        let form = LxcCreateForm { vmid: 100, request: &request };
        let value = serde_json::to_value(form).unwrap();
        assert_eq!(value["vmid"], 100);
        assert_eq!(value["ostemplate"], "local:vztmpl/debian-12.tar.zst");
        assert_eq!(value["memory"], 1024);
        assert_eq!(value["net0"], "name=eth0,bridge=vmbr0");
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
}
