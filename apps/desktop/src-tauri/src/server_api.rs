//! Typed HTTP client for the BLT server's game-distribution and shared-pool
//! services (TDD §5, §5.1).

use blt_core::manifest::Manifest;
use blt_core::protocol::{ShareListing, ShareSummary, TitleInfo, TitleSummary};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not connected to a server")]
    NotConnected,
    #[error("http {0}")]
    Status(u16),
    #[error("share was deleted on the server")]
    ShareGone,
    #[error("network: {0}")]
    Network(String),
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Network(e.to_string())
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default()
}

fn check(status: reqwest::StatusCode) -> Result<(), ApiError> {
    if status == reqwest::StatusCode::GONE {
        return Err(ApiError::ShareGone);
    }
    if !status.is_success() {
        return Err(ApiError::Status(status.as_u16()));
    }
    Ok(())
}

// ── game distribution ──

pub async fn list_titles(game: &str) -> Result<Vec<TitleSummary>, ApiError> {
    let r = http().get(format!("http://{game}/titles")).send().await?;
    check(r.status())?;
    Ok(r.json().await?)
}

pub async fn title_info(game: &str, id: u64) -> Result<TitleInfo, ApiError> {
    let r = http()
        .get(format!("http://{game}/titles/{id}/info"))
        .send()
        .await?;
    check(r.status())?;
    Ok(r.json().await?)
}

pub async fn title_manifest(game: &str, id: u64) -> Result<Manifest, ApiError> {
    let r = http()
        .get(format!("http://{game}/titles/{id}/manifest"))
        .send()
        .await?;
    check(r.status())?;
    Ok(r.json().await?)
}

/// Fetch one chunk from any chunk-serving endpoint (server or peer — the same
/// path shape, TDD §7).
pub async fn fetch_chunk(endpoint: &str, file_id: u64, idx: u32) -> Result<Vec<u8>, ApiError> {
    let r = http()
        .get(format!("http://{endpoint}/chunks/{file_id}/{idx}"))
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    check(r.status())?;
    Ok(r.bytes().await?.to_vec())
}

/// Fetch the title's Windows post-install script (bytes + server-stated hash).
pub async fn title_script(game: &str, id: u64) -> Result<Option<(Vec<u8>, String)>, ApiError> {
    let r = http()
        .get(format!("http://{game}/titles/{id}/script"))
        .send()
        .await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    check(r.status())?;
    let hash = r
        .headers()
        .get("x-blt-hash")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    Ok(Some((r.bytes().await?.to_vec(), hash)))
}

/// Verify the admin password with the server (used by the playback-lockdown
/// gate, F14.3 — the client never stores the hash).
pub async fn verify_admin_password(game: &str, password: &str) -> Result<bool, ApiError> {
    let r = http()
        .post(format!("http://{game}/admin/verify"))
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await?;
    if r.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(false);
    }
    check(r.status())?;
    Ok(true)
}

// ── shared pool ──

pub async fn list_shares(share: &str) -> Result<Vec<ShareSummary>, ApiError> {
    let r = http().get(format!("http://{share}/shares")).send().await?;
    check(r.status())?;
    Ok(r.json().await?)
}

pub async fn share_listing(share: &str, id: u64) -> Result<ShareListing, ApiError> {
    let r = http()
        .get(format!("http://{share}/shares/{id}"))
        .send()
        .await?;
    check(r.status())?;
    Ok(r.json().await?)
}

/// Download one file of a share to `dest` (streamed). Returns bytes written.
pub async fn download_share_file(
    share: &str,
    id: u64,
    rel: &str,
    dest: &std::path::Path,
) -> Result<u64, ApiError> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let encoded = rel.split('/').map(urlencode).collect::<Vec<_>>().join("/");
    let r = http()
        .get(format!("http://{share}/shares/{id}/files/{encoded}"))
        .timeout(Duration::from_secs(3600))
        .send()
        .await?;
    check(r.status())?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ApiError::Network(format!("mkdir: {e}")))?;
    }
    let mut f = tokio::fs::File::create(dest)
        .await
        .map_err(|e| ApiError::Network(format!("create: {e}")))?;
    let mut stream = r.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        f.write_all(&bytes)
            .await
            .map_err(|e| ApiError::Network(format!("write: {e}")))?;
        written += bytes.len() as u64;
    }
    f.flush()
        .await
        .map_err(|e| ApiError::Network(format!("flush: {e}")))?;
    Ok(written)
}

/// Upload files as one share. `files` are `(absolute_path, relative_path)`;
/// folder shares carry their tree in the relative paths.
pub async fn upload_share(
    share: &str,
    client_id: &str,
    name: &str,
    kind: &str,
    owner_name: &str,
    files: &[(std::path::PathBuf, String)],
) -> Result<ShareSummary, ApiError> {
    let mut form = reqwest::multipart::Form::new().part(
        "meta",
        reqwest::multipart::Part::text(
            serde_json::json!({ "name": name, "kind": kind, "owner_name": owner_name }).to_string(),
        ),
    );
    for (abs, rel) in files {
        let bytes = tokio::fs::read(abs)
            .await
            .map_err(|e| ApiError::Network(format!("read {}: {e}", abs.display())))?;
        form = form.part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name(rel.clone()),
        );
    }
    let r = http()
        .post(format!("http://{share}/shares"))
        .header("x-blt-client-id", client_id)
        .multipart(form)
        .timeout(Duration::from_secs(3600))
        .send()
        .await?;
    check(r.status())?;
    Ok(r.json().await?)
}

pub async fn delete_share(share: &str, client_id: &str, id: u64) -> Result<(), ApiError> {
    let r = http()
        .delete(format!("http://{share}/shares/{id}"))
        .header("x-blt-client-id", client_id)
        .send()
        .await?;
    check(r.status())?;
    Ok(())
}

fn urlencode(s: &str) -> String {
    // Minimal percent-encoding for path segments.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'('
            | b')'
            | b' ' => {
                if b == b' ' {
                    out.push_str("%20");
                } else {
                    out.push(b as char);
                }
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
