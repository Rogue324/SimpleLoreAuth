use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand_core::OsRng;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};

use crate::db::User;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceClaim {
    pub resource_id: String,
    pub permission: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    #[serde(rename = "sub")]
    pub user_id: String,
    #[serde(rename = "iss")]
    pub issuer: String,
    #[serde(rename = "iat")]
    pub issued_at: u64,
    #[serde(rename = "exp")]
    pub expires: u64,
    #[serde(rename = "aud")]
    pub audience: Vec<String>,
    pub env: String,
    pub name: String,
    pub preferred_username: String,
    pub resources: Option<Vec<ResourceClaim>>,
    pub groups: Option<Vec<String>>,
    pub is_service_account: Option<bool>,
    pub idp: String,
    pub token_use: String,
}

#[derive(Clone)]
pub struct TokenIssuer {
    encoding: EncodingKey,
    decoding: DecodingKey,
    kid: String,
    issuer: String,
    audience: String,
    environment: String,
    ttl_seconds: u64,
    jwks: serde_json::Value,
}

impl TokenIssuer {
    pub fn load_or_create(
        key_path: impl AsRef<Path>,
        issuer: String,
        audience: String,
        environment: String,
        ttl_seconds: u64,
    ) -> Result<Self> {
        let key_path = key_path.as_ref();
        let private = load_or_create_private_key(key_path)?;
        let public = RsaPublicKey::from(&private);
        let private_pem = private.to_pkcs8_pem(LineEnding::LF)?;
        let public_pem = public.to_public_key_pem(LineEnding::LF)?;
        let public_der = public.to_public_key_der()?;
        let kid = blake3::hash(public_der.as_bytes()).to_hex()[..16].to_string();
        let n = URL_SAFE_NO_PAD.encode(public.n().to_bytes_be());
        let e = URL_SAFE_NO_PAD.encode(public.e().to_bytes_be());
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": kid,
                "n": n,
                "e": e
            }]
        });
        Ok(Self {
            encoding: EncodingKey::from_rsa_pem(private_pem.as_bytes())?,
            decoding: DecodingKey::from_rsa_pem(public_pem.as_bytes())?,
            kid,
            issuer,
            audience,
            environment,
            ttl_seconds,
            jwks,
        })
    }

    pub fn issue_authentication(&self, user: &User) -> Result<(String, u64)> {
        self.issue(user, None, "authentication")
    }

    pub fn issue_authorization(
        &self,
        user: &User,
        resources: Vec<ResourceClaim>,
    ) -> Result<(String, u64)> {
        self.issue(user, Some(resources), "authorization")
    }

    fn issue(
        &self,
        user: &User,
        resources: Option<Vec<ResourceClaim>>,
        token_use: &str,
    ) -> Result<(String, u64)> {
        let now = chrono::Utc::now().timestamp() as u64;
        let expires = now.saturating_add(self.ttl_seconds);
        let claims = Claims {
            user_id: user.id.clone(),
            issuer: self.issuer.clone(),
            issued_at: now,
            expires,
            audience: vec![self.audience.clone()],
            env: self.environment.clone(),
            name: user.display_name.clone(),
            preferred_username: user.username.clone(),
            resources,
            groups: if user.is_admin {
                Some(vec!["admin".into()])
            } else {
                None
            },
            is_service_account: Some(false),
            idp: "lore-auth".into(),
            token_use: token_use.into(),
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        Ok((encode(&header, &claims, &self.encoding)?, expires))
    }

    pub fn verify(&self, token: &str) -> Result<Claims> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.validate_exp = true;
        Ok(decode::<Claims>(token, &self.decoding, &validation)?.claims)
    }

    pub fn jwks(&self) -> serde_json::Value {
        self.jwks.clone()
    }
}

fn load_or_create_private_key(path: &Path) -> Result<RsaPrivateKey> {
    if path.exists() {
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading private key {}", path.display()))?;
        return RsaPrivateKey::from_pkcs8_pem(&pem).context("parsing RSA private key");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let key = RsaPrivateKey::new(&mut OsRng, 3072)?;
    let pem = key.to_pkcs8_pem(LineEnding::LF)?;
    write_private_key(path.to_path_buf(), pem.as_bytes())?;
    Ok(key)
}

fn write_private_key(path: PathBuf, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("creating private key {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
