use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use hkdf::Hkdf;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ED25519, SanType,
};
use rustls_pki_types::PrivatePkcs8KeyDer;
use sha2::{Digest, Sha256};
use std::fmt;
use thiserror::Error;
use time::{Duration, OffsetDateTime};

const CONTEXT_DOMAIN: &[u8] = b"pbox.cwd.dev/context/v1";
const CONTEXT_INFO: &[u8] = b"pbox control CA";

#[derive(Clone, PartialEq, Eq)]
pub struct ContextSeed([u8; 32]);

impl fmt::Debug for ContextSeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted context seed>")
    }
}

impl AsRef<[u8]> for ContextSeed {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub fn derive_context_seed(token_id: &str, token_secret: &str) -> ContextSeed {
    let mut salt_input = Vec::with_capacity(CONTEXT_DOMAIN.len() + token_id.len());
    salt_input.extend_from_slice(CONTEXT_DOMAIN);
    salt_input.extend_from_slice(token_id.as_bytes());
    let salt = Sha256::digest(&salt_input);
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), token_secret.as_bytes());
    let mut seed = [0u8; 32];
    hkdf.expand(CONTEXT_INFO, &mut seed)
        .expect("32-byte HKDF output is valid for SHA-256");
    ContextSeed(seed)
}

pub fn context_fingerprint(seed: &ContextSeed) -> String {
    let digest = Sha256::digest(seed.as_ref());
    URL_SAFE_NO_PAD.encode(&digest[..16])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificatePurpose {
    Client,
    Server,
}

#[derive(Clone)]
pub struct CertificateMaterial {
    pub certificate_pem: String,
    pub certificate_der: Vec<u8>,
    pub private_key_pem: String,
    pub private_key_der: Vec<u8>,
    pub chain_pem: Option<String>,
}

impl fmt::Debug for CertificateMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CertificateMaterial")
            .field("certificate_pem", &"<redacted certificate>")
            .field("certificate_der", &"<redacted certificate>")
            .field("private_key_pem", &"<redacted private key>")
            .field("private_key_der", &"<redacted private key>")
            .field(
                "chain_pem",
                &self.chain_pem.as_ref().map(|_| "<redacted chain>"),
            )
            .finish()
    }
}

pub fn generate_context_ca(seed: &ContextSeed) -> Result<CertificateMaterial, CryptoError> {
    let signing_key = SigningKey::from_bytes(seed.as_ref().try_into().expect("fixed seed length"));
    let pkcs8 = signing_key
        .to_pkcs8_der()
        .map_err(|error| CryptoError::KeyEncoding(error.to_string()))?;
    let private_key = PrivatePkcs8KeyDer::from(pkcs8.as_bytes());
    let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(&private_key, &PKCS_ED25519)
        .map_err(|error| CryptoError::Certificate(error.to_string()))?;

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "pbox context CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|error| CryptoError::Certificate(error.to_string()))?;
    Ok(material_from_certificate(certificate, key_pair, None))
}
fn context_ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "pbox context CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

pub fn issue_certificate(
    ca: &CertificateMaterial,
    subject: &str,
    purpose: CertificatePurpose,
) -> Result<CertificateMaterial, CryptoError> {
    let ca_key = KeyPair::from_pem(&ca.private_key_pem)
        .map_err(|error| CryptoError::Certificate(error.to_string()))?;
    let issuer = Issuer::new(context_ca_params(), ca_key);
    let leaf_key = KeyPair::generate_for(&PKCS_ED25519)
        .map_err(|error| CryptoError::Certificate(error.to_string()))?;
    let mut params = CertificateParams::default();
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::minutes(1);
    params.not_after = match purpose {
        CertificatePurpose::Client => now + Duration::minutes(5),
        CertificatePurpose::Server => now + Duration::days(365),
    };
    params.distinguished_name.push(DnType::CommonName, subject);
    params.subject_alt_names.push(SanType::URI(
        subject
            .try_into()
            .map_err(|error| CryptoError::Certificate(format!("invalid SAN: {error}")))?,
    ));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![match purpose {
        CertificatePurpose::Client => ExtendedKeyUsagePurpose::ClientAuth,
        CertificatePurpose::Server => ExtendedKeyUsagePurpose::ServerAuth,
    }];
    let certificate = params
        .signed_by(&leaf_key, &issuer)
        .map_err(|error| CryptoError::Certificate(error.to_string()))?;
    Ok(material_from_certificate(
        certificate,
        leaf_key,
        Some(ca.certificate_pem.clone()),
    ))
}

pub fn server_subject(box_id: &str) -> Result<String, CryptoError> {
    if !box_id.starts_with("pbx_") || box_id.len() != 12 || !box_id[4..].bytes().all(is_id_byte) {
        return Err(CryptoError::InvalidBoxId(box_id.to_owned()));
    }
    Ok(format!("pbox.cwd.dev/box/{box_id}"))
}

fn is_id_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn material_from_certificate(
    certificate: Certificate,
    key_pair: KeyPair,
    chain_pem: Option<String>,
) -> CertificateMaterial {
    CertificateMaterial {
        certificate_pem: certificate.pem(),
        certificate_der: certificate.der().as_ref().to_vec(),
        private_key_pem: key_pair.serialize_pem(),
        private_key_der: key_pair.serialize_der(),
        chain_pem,
    }
}

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("certificate operation failed: {0}")]
    Certificate(String),
    #[error("private key encoding failed: {0}")]
    KeyEncoding(String),
    #[error("invalid pbox box id: {0}")]
    InvalidBoxId(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_derivation_is_deterministic_and_url_independent() {
        let first = derive_context_seed("pbox@pve!cli", "secret");
        let second = derive_context_seed("pbox@pve!cli", "secret");
        let other = derive_context_seed("pbox@pve!cli", "other");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(context_fingerprint(&first), context_fingerprint(&second));
    }

    #[test]
    fn context_seed_debug_does_not_expose_bytes() {
        let seed = derive_context_seed("pbox@pve!cli", "secret");
        let rendered = format!("{seed:?}");
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn issued_certificates_have_distinct_server_identity_material() {
        let seed = derive_context_seed("pbox@pve!cli", "secret");
        let ca = generate_context_ca(&seed).unwrap();
        let first = issue_certificate(
            &ca,
            &server_subject("pbx_t3yzd9y3").unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        let second = issue_certificate(
            &ca,
            &server_subject("pbx_91mk2aa7").unwrap(),
            CertificatePurpose::Server,
        )
        .unwrap();
        assert_ne!(first.private_key_der, second.private_key_der);
        assert_ne!(first.certificate_der, second.certificate_der);
        assert!(first.chain_pem.is_some());
    }

    #[test]
    fn server_subject_rejects_invalid_ids() {
        assert!(server_subject("pbx_bad!").is_err());
        assert!(server_subject("pbx_t3yzd9y3").is_ok());
    }
}
