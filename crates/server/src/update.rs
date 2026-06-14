//! Server self-update (ENH-2). Mirrors the desktop app's manual-update model
//! (HARD CONSTRAINT #2): the admin panel can *check* GitHub Releases and, on an
//! explicit confirmed click, *download + restart*. Never automatic.
//!
//! The auto-install replaces the running binary in place and re-execs
//! (`crate::reexec`). On macOS this is clean (rename over the running inode);
//! on Windows a running `.exe` can't be replaced, so install returns an error
//! pointing at the manual swap. The **check** works on every platform.

use serde::Serialize;

/// The canonical release source — must match the desktop updater endpoint and
/// the CI release repo.
const REPO: &str = "rickenwing/BLT";

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Parse `vX.Y.Z` / `X.Y.Z` (ignoring any pre-release suffix) into a tuple.
fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it
        .next()
        .unwrap_or("0")
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("0")
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

/// Is `latest` strictly newer than `current`? Unparseable → not newer (safe).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_ver(latest), parse_ver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub notes: String,
    pub url: String,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("blt-server")
        .build()
        .unwrap_or_default()
}

async fn latest_release() -> anyhow::Result<serde_json::Value> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = http()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// Check for a newer published release (manual indicator, F0.4-style).
pub async fn check() -> anyhow::Result<UpdateInfo> {
    let v = latest_release().await?;
    let tag = v["tag_name"].as_str().unwrap_or_default().to_string();
    let current = current_version();
    Ok(UpdateInfo {
        update_available: is_newer(&tag, &current),
        latest: (!tag.is_empty()).then_some(tag),
        notes: v["body"].as_str().unwrap_or_default().to_string(),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        current,
    })
}

/// The server tarball asset name for the current OS (matches `release.yml`).
fn asset_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "blt-server-windows.tar.gz"
    } else {
        "blt-server-macos.tar.gz"
    }
}

/// Download the latest server tarball and replace the running install in place.
/// Does **all** fallible work (download → extract → validate) before the commit
/// point (rename over the running binary), so a bad download can't brick the
/// install. The caller re-execs after this returns Ok.
pub async fn install() -> anyhow::Result<()> {
    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "in-place server update isn't supported on Windows (a running .exe can't be \
             replaced) — download {} from the release page and swap it manually",
            asset_name()
        );
    }
    #[cfg(unix)]
    {
        let rel = latest_release().await?;
        let tag = rel["tag_name"].as_str().unwrap_or_default();
        if !is_newer(tag, &current_version()) {
            anyhow::bail!("already on the latest version ({})", current_version());
        }
        let want = asset_name();
        let url = rel["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(want)))
            .and_then(|x| x["browser_download_url"].as_str())
            .ok_or_else(|| anyhow::anyhow!("release {tag} has no asset {want}"))?
            .to_string();

        let bytes = http()
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;

        let exe = std::env::current_exe()?;
        let dir = exe
            .parent()
            .ok_or_else(|| anyhow::anyhow!("can't resolve install dir"))?
            .to_path_buf();
        let staging = dir.join(".blt-update");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;

        // Extract the tarball into the staging dir.
        let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(&bytes[..]));
        tar::Archive::new(gz).unpack(&staging)?;

        let new_bin = staging.join("blt-server");
        if !new_bin.is_file() {
            anyhow::bail!("downloaded tarball is missing blt-server");
        }
        validate_binary(&new_bin)?;

        // ── commit point ── replace the binary (rename keeps the running inode
        // alive until re-exec) and the adjacent admin-web SPA.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&new_bin, &exe)?;
        let new_spa = staging.join("admin-web");
        if new_spa.is_dir() {
            let target = dir.join("admin-web");
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&new_spa, &target)?;
        }
        let _ = std::fs::remove_dir_all(&staging);
        tracing::info!(%tag, "server update staged; re-exec pending");
        Ok(())
    }
}

/// Sanity-check the downloaded binary actually runs and reports a parseable
/// version, so a corrupt/incompatible download is rejected before we commit.
#[cfg(unix)]
fn validate_binary(bin: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))?;
    let out = std::process::Command::new(bin).arg("--version").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ver = text.split_whitespace().last().unwrap_or_default();
    if parse_ver(ver).is_none() {
        anyhow::bail!("new binary failed --version sanity check");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "v0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(!is_newer("garbage", "0.1.1"));
        // pre-release suffix on patch is tolerated
        assert!(is_newer("v0.1.2-rc1", "0.1.1"));
    }
}
