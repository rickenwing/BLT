//! Post-install scripts (F16, HARD CONSTRAINT #15): Windows-only, canonical-
//! library-only, run **after download + validation**, and only after the user
//! has **seen the script contents and explicitly confirmed**. Never auto-run.

use crate::server_api;
use crate::state::Shared;
use serde::Serialize;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize)]
pub struct ScriptPreview {
    pub contents: String,
    pub hash: String,
    /// Scripts execute on Windows only; other platforms can view but not run.
    pub runnable_here: bool,
}

/// Fetch the script for display (the confirmation step shows this, F16.4).
pub async fn fetch_preview(state: &Shared, title_id: u64) -> Result<Option<ScriptPreview>, String> {
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or("not connected")?;
    let Some((bytes, hash)) = server_api::title_script(&game, title_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    // Verify the script against the server-stated hash before showing it.
    if blt_core::hash_bytes(&bytes).to_hex() != hash {
        return Err("script hash mismatch — refusing".into());
    }
    Ok(Some(ScriptPreview {
        contents: String::from_utf8_lossy(&bytes).into_owned(),
        hash,
        runnable_here: cfg!(target_os = "windows"),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub output: String,
}

/// Run the (already previewed + confirmed) script in the title's install dir.
/// `expected_hash` must match what the user saw — the confirmation covers
/// exactly these bytes. Windows-only (F16.1); a failure leaves the downloaded
/// files intact (F16.5).
pub async fn run_confirmed(
    state: &Shared,
    title_id: u64,
    install_dir: &std::path::Path,
    expected_hash: &str,
) -> Result<ScriptResult, String> {
    if !cfg!(target_os = "windows") {
        return Err("post-install scripts run on Windows only".into());
    }
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or("not connected")?;
    let Some((bytes, hash)) = server_api::title_script(&game, title_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Err("script no longer available".into());
    };
    if hash != expected_hash || blt_core::hash_bytes(&bytes).to_hex() != hash {
        return Err("script changed since it was confirmed — re-review required".into());
    }

    // Write next to the install dir and execute with the OS shell.
    let script_path = install_dir.join(".blt-install.ps1");
    tokio::fs::write(&script_path, &bytes)
        .await
        .map_err(|e| format!("write script: {e}"))?;

    info!(title = title_id, dir = %install_dir.display(), "running post-install script");
    let output = tokio::process::Command::new("powershell")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script_path)
        .current_dir(install_dir)
        .output()
        .await
        .map_err(|e| format!("spawn: {e}"))?;

    let _ = tokio::fs::remove_file(&script_path).await;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = ScriptResult {
        success: output.status.success(),
        exit_code: output.status.code(),
        output: combined,
    };
    if result.success {
        info!(title = title_id, "post-install script succeeded");
    } else {
        // Failure leaves files intact and reports the error (F16.5).
        warn!(title = title_id, code = ?result.exit_code, "post-install script failed");
    }
    Ok(result)
}
