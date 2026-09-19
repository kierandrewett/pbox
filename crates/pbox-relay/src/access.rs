//! PVE visibility grants a short-lived identity for one administrator-enrolled box.
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use pbox_crypto::{
    CertificateMaterial, CertificatePurpose, derive_context_seed, generate_context_ca,
    issue_certificate, server_subject,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

/// Administrator-owned bindings. PVE credentials are never stored here.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub pve_url: String,
    pub boxes: BTreeMap<String, u64>,
}

struct Issuer {
    settings: Settings,
    master: String,
    http: reqwest::Client,
    slots: tokio::sync::Semaphore,
}

/// Contains only a leaf private key; never serialises the signing key.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub ca_pem: String,
    pub certificate_pem: String,
    pub private_key_pem: String,
    pub relay_token: String,
    pub expires_at: i64,
}

impl Credentials {
    pub fn identity(&self) -> Result<CertificateMaterial> {
        let certificate = pem::parse(&self.certificate_pem)?;
        let key = pem::parse(&self.private_key_pem)?;
        ensure!(
            certificate.tag() == "CERTIFICATE" && key.tag() == "PRIVATE KEY",
            "invalid credential encoding"
        );
        Ok(CertificateMaterial {
            certificate_pem: self.certificate_pem.clone(),
            certificate_der: certificate.contents().to_vec(),
            private_key_pem: self.private_key_pem.clone(),
            private_key_der: key.contents().to_vec(),
            chain_pem: Some(self.ca_pem.clone()),
        })
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.pve_url)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && matches!(url.path(), "" | "/"),
            "pve_url must be a trusted HTTPS origin"
        );
        let mut vmids = BTreeSet::new();
        for (id, vmid) in &self.boxes {
            ensure!(
                crate::valid_box_id(id) && *vmid > 0 && vmids.insert(vmid),
                "invalid or duplicate box binding"
            );
        }
        Ok(())
    }
}

fn ca(master: &str, id: &str) -> Result<CertificateMaterial> {
    let seed = derive_context_seed(&format!("pbox-relay-authority-v2/{id}"), master);
    Ok(generate_context_ca(&seed)?)
}

/// Export on the trusted relay host, then transfer through verified host access.
pub fn guest_identity(master: &str, id: &str) -> Result<(CertificateMaterial, String)> {
    let authority = ca(master, id)?;
    let server = issue_certificate(&authority, &server_subject(id)?, CertificatePurpose::Server)?;
    Ok((server, authority.certificate_pem))
}

pub fn router(settings: Settings, master: String) -> Result<Router> {
    settings.validate()?;
    ensure!(master.len() >= 32, "relay key is too short");
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(Router::new()
        .route("/v2/access/{box_id}", post(issue))
        .with_state(Arc::new(Issuer {
            settings,
            master,
            http,
            slots: tokio::sync::Semaphore::new(32),
        })))
}

#[derive(Deserialize)]
struct Resources {
    data: Vec<Resource>,
}
#[derive(Deserialize)]
struct Resource {
    vmid: Option<u64>,
    #[serde(rename = "type")]
    kind: String,
}

async fn issue(
    State(state): State<Arc<Issuer>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(vmid) = state.settings.boxes.get(&id) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(auth) = headers
        .get("authorization")
        .filter(|value| value.to_str().is_ok_and(|v| v.starts_with("PVEAPIToken=")))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(_permit) = state.slots.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let result = state
        .http
        .get(format!(
            "{}/api2/json/cluster/resources?type=vm",
            state.settings.pve_url.trim_end_matches('/')
        ))
        .header("authorization", auth.clone())
        .send()
        .await;
    let response = match result {
        Ok(response) if response.status().is_success() => response,
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
            return StatusCode::FORBIDDEN.into_response();
        }
        _ => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let resources = match response.json::<Resources>().await {
        Ok(resources) => resources,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    if !resources
        .data
        .iter()
        .any(|r| r.vmid == Some(*vmid) && matches!(r.kind.as_str(), "lxc" | "qemu"))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    match issue_credentials(&state.master, &id) {
        Ok(credentials) => ([("cache-control", "no-store")], Json(credentials)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn issue_credentials(master: &str, id: &str) -> Result<Credentials> {
    let authority = ca(master, id)?;
    let identity = issue_certificate(
        &authority,
        "pbox.cwd.dev/pve-visible",
        CertificatePurpose::Client,
    )?;
    let expires_at = time::OffsetDateTime::now_utc().unix_timestamp() + 300;
    let signature = crate::scoped_token(master, "client-expiring", &format!("{id}/{expires_at}"));
    Ok(Credentials {
        ca_pem: authority.certificate_pem,
        certificate_pem: identity.certificate_pem,
        private_key_pem: identity.private_key_pem,
        relay_token: format!("v2.{expires_at}.{signature}"),
        expires_at,
    })
}

pub(crate) fn authorised(master: &str, id: &str, token: &str, now: i64) -> bool {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || parts[0] != "v2" {
        return false;
    }
    let Ok(expiry) = parts[1].parse::<i64>() else {
        return false;
    };
    if expiry <= now || expiry > now.saturating_add(300) {
        return false;
    }
    crate::authorised(
        master,
        "client-expiring",
        &format!("{id}/{expiry}"),
        parts[2],
    )
}

/// Fetch credentials over verified HTTPS. The configured relay is the trusted issuer.
pub async fn request(
    origin: &str,
    id: &str,
    token_id: &str,
    token_secret: &str,
) -> Result<Credentials> {
    let url = crate::websocket_url(origin, "client", id)?;
    ensure!(
        url.starts_with("wss://"),
        "PVE authentication requires an HTTPS relay"
    );
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?
        .post(format!("{}/v2/access/{id}", origin.trim_end_matches('/')))
        .header(
            "authorization",
            format!("PVEAPIToken={token_id}={token_secret}"),
        )
        .send()
        .await
        .context("contact PVE access issuer")?;
    ensure!(
        response.status().is_success(),
        "PVE access issuer returned {}; check VM visibility and relay enrolment",
        response.status()
    );
    response
        .json()
        .await
        .context("decode PVE access credentials")
}

#[cfg(test)]
mod tests {
    use super::*;
    const MASTER: &str = "test-master-key-with-at-least-32-characters";
    #[tokio::test]
    async fn issuer_requires_current_visibility_and_never_follows_redirects() {
        use axum::routing::get;
        let pve = Router::new().route(
            "/api2/json/cluster/resources",
            get(|headers: HeaderMap| async move {
                match headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                {
                    "PVEAPIToken=visible" => {
                        Json(serde_json::json!({"data":[{"vmid":9002,"type":"lxc"}]}))
                            .into_response()
                    }
                    "PVEAPIToken=hidden" => {
                        Json(serde_json::json!({"data":[{"vmid":9003,"type":"lxc"}]}))
                            .into_response()
                    }
                    "PVEAPIToken=redirect" => (
                        StatusCode::FOUND,
                        [("location", "http://127.0.0.1:1/steal")],
                    )
                        .into_response(),
                    "PVEAPIToken=malformed" => {
                        Json(serde_json::json!({"unexpected":[]})).into_response()
                    }
                    _ => StatusCode::UNAUTHORIZED.into_response(),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pve_url = format!("http://{}", listener.local_addr().unwrap());
        let pve_task = tokio::spawn(async move {
            axum::serve(listener, pve).await.unwrap();
        });
        // Only this test constructs an HTTP issuer; production validates HTTPS.
        let issuer = Router::new()
            .route("/v2/access/{box_id}", post(issue))
            .with_state(Arc::new(Issuer {
                settings: Settings {
                    pve_url,
                    boxes: BTreeMap::from([("pbx_12345678".into(), 9002)]),
                },
                master: MASTER.into(),
                http: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                slots: tokio::sync::Semaphore::new(2),
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let issuer_task = tokio::spawn(async move {
            axum::serve(listener, issuer).await.unwrap();
        });
        let client = reqwest::Client::new();
        for (credential, expected) in [
            ("visible", 200),
            ("hidden", 403),
            ("revoked", 403),
            ("redirect", 502),
            ("malformed", 502),
        ] {
            let response = client
                .post(format!("{origin}/v2/access/pbx_12345678"))
                .header("authorization", format!("PVEAPIToken={credential}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected, "{credential}");
            if expected == 200 {
                assert_eq!(response.headers()["cache-control"], "no-store");
                let c: Credentials = response.json().await.unwrap();
                assert!(authorised(
                    MASTER,
                    "pbx_12345678",
                    &c.relay_token,
                    c.expires_at - 1
                ));
            }
        }
        let response = client
            .post(format!("{origin}/v2/access/pbx_87654321"))
            .header("authorization", "PVEAPIToken=visible")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        pve_task.abort();
        issuer_task.abort();
    }
    #[test]
    fn credentials_expire_and_cannot_cross_boxes() {
        let c = issue_credentials(MASTER, "pbx_12345678").unwrap();
        assert!(authorised(
            MASTER,
            "pbx_12345678",
            &c.relay_token,
            c.expires_at - 1
        ));
        assert!(!authorised(
            MASTER,
            "pbx_87654321",
            &c.relay_token,
            c.expires_at - 1
        ));
        assert!(!authorised(
            MASTER,
            "pbx_12345678",
            &c.relay_token,
            c.expires_at
        ));
        assert!(!authorised(
            "different-master",
            "pbx_12345678",
            &c.relay_token,
            c.expires_at - 1
        ));
        assert_ne!(
            ca(MASTER, "pbx_12345678").unwrap().private_key_der,
            ca(MASTER, "pbx_87654321").unwrap().private_key_der
        );
        assert!(c.identity().is_ok());
    }
    #[test]
    fn settings_reject_untrusted_origins_and_duplicate_vmids() {
        let mut s = Settings {
            pve_url: "https://pve.example".into(),
            boxes: BTreeMap::from([("pbx_12345678".into(), 9002)]),
        };
        assert!(s.validate().is_ok());
        s.pve_url = "http://pve.example".into();
        assert!(s.validate().is_err());
        s.pve_url = "https://user@pve.example".into();
        assert!(s.validate().is_err());
        s.pve_url = "https://pve.example".into();
        s.boxes.insert("pbx_87654321".into(), 9002);
        assert!(s.validate().is_err());
    }
}
