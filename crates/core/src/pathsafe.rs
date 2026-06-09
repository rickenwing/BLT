//! Cross-platform path sanitisation for **received** shared-pool names.
//!
//! HARD CONSTRAINT #11 / F15 cross-cutting: a folder authored on one OS must
//! write **safely** on another. Sanitisation happens on the **writing side**.
//! We never write an untrusted relative path verbatim. This module:
//!
//! - rejects path traversal (`..`) and absolute / drive-letter paths,
//! - transforms characters illegal on the target OS (`<>:"|?*`, control chars),
//! - strips Windows-hostile trailing dots/spaces,
//! - escapes reserved Windows device names (CON, PRN, NUL, COM1…, LPT1…),
//! - enforces a conservative length cap,
//! - and offers case-insensitive collision resolution for a write set.

use std::collections::HashSet;

/// Conservative cap on a sanitised relative path length (bytes). Leaves room
/// under the Windows ~260 `MAX_PATH` once a real install root is prepended.
pub const MAX_REL_PATH_LEN: usize = 230;
/// Per-component cap (Windows/most filesystems allow 255).
pub const MAX_COMPONENT_LEN: usize = 255;

/// Why a relative path could not be made safe.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathSafetyError {
    #[error("path contains a traversal component ('..')")]
    Traversal,
    #[error("path is absolute or names a drive")]
    Absolute,
    #[error("path is empty after sanitisation")]
    Empty,
    #[error("path is too long ({len} > {max} bytes)")]
    TooLong { len: usize, max: usize },
}

const ILLEGAL: &[char] = &['<', '>', ':', '"', '|', '?', '*', '/', '\\'];

fn is_reserved_windows_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.starts_with("COM") && tail_is_digit_1_9(&upper[3..]))
        || (upper.starts_with("LPT") && tail_is_digit_1_9(&upper[3..]))
}

fn tail_is_digit_1_9(s: &str) -> bool {
    s.len() == 1 && matches!(s.as_bytes()[0], b'1'..=b'9')
}

/// Sanitise a single path component (no separators expected inside).
///
/// Returns `None` for `.`/empty (caller should skip) and `Some(name)` otherwise.
/// `..` is **not** handled here — [`sanitize_rel_path`] rejects it before this.
pub fn sanitize_component(raw: &str) -> Option<String> {
    if raw.is_empty() || raw == "." {
        return None;
    }
    // Replace illegal characters and control chars with '_'.
    let mut s: String = raw
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Windows strips trailing dots and spaces; do it ourselves so names match.
    let trimmed = s.trim_end_matches([' ', '.']);
    if trimmed.len() != s.len() {
        s = trimmed.to_string();
    }
    if s.is_empty() {
        s = "_".to_string();
    }

    // Escape reserved device names (compare the stem before the first dot).
    let stem = s.split('.').next().unwrap_or(&s);
    if is_reserved_windows_name(stem) {
        s = format!("_{s}");
    }

    // Cap component length, preserving an extension where possible.
    if s.len() > MAX_COMPONENT_LEN {
        s = truncate_keep_ext(&s, MAX_COMPONENT_LEN);
    }

    Some(s)
}

fn truncate_keep_ext(s: &str, max: usize) -> String {
    match s.rsplit_once('.') {
        Some((stem, ext)) if ext.len() + 1 < max && !ext.is_empty() => {
            let keep = max - ext.len() - 1;
            let mut out: String = stem.chars().take(keep).collect();
            out.push('.');
            out.push_str(ext);
            out
        }
        _ => s.chars().take(max).collect(),
    }
}

/// Sanitise a relative path (from an upload's folder structure) into a safe,
/// forward-slash-separated relative path that cannot escape the target dir.
pub fn sanitize_rel_path(raw: &str) -> Result<String, PathSafetyError> {
    // Accept both separators; a folder authored on Windows carries '\'.
    let parts = raw.split(['/', '\\']);
    let mut out: Vec<String> = Vec::new();
    for (i, part) in parts.enumerate() {
        if part == ".." {
            return Err(PathSafetyError::Traversal);
        }
        // A drive letter like "C:" only as the first component => absolute.
        if i == 0
            && part.len() == 2
            && part.ends_with(':')
            && part.as_bytes()[0].is_ascii_alphabetic()
        {
            return Err(PathSafetyError::Absolute);
        }
        if let Some(c) = sanitize_component(part) {
            out.push(c);
        }
    }
    if out.is_empty() {
        return Err(PathSafetyError::Empty);
    }
    let joined = out.join("/");
    if joined.len() > MAX_REL_PATH_LEN {
        return Err(PathSafetyError::TooLong {
            len: joined.len(),
            max: MAX_REL_PATH_LEN,
        });
    }
    Ok(joined)
}

/// Resolve a case-insensitive collision against an existing write set.
///
/// Windows/macOS default filesystems are case-insensitive, so two entries that
/// differ only in case would clobber each other. Given the set of already-used
/// paths (compared lowercased) and a `candidate`, returns a unique path,
/// appending ` (2)`, ` (3)`, … to the final component's stem as needed.
/// The chosen path is inserted into `used` (lowercased).
pub fn resolve_collision(used: &mut HashSet<String>, candidate: &str) -> String {
    let key = candidate.to_lowercase();
    if used.insert(key) {
        return candidate.to_string();
    }
    let (dir, name) = match candidate.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, candidate),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_string())),
        _ => (name.to_string(), None),
    };
    for n in 2..u32::MAX {
        let new_name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let new_path = match dir {
            Some(d) => format!("{d}/{new_name}"),
            None => new_name,
        };
        if used.insert(new_path.to_lowercase()) {
            return new_path;
        }
    }
    candidate.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        assert_eq!(
            sanitize_rel_path("../etc/passwd"),
            Err(PathSafetyError::Traversal)
        );
        assert_eq!(
            sanitize_rel_path("a/../../b"),
            Err(PathSafetyError::Traversal)
        );
        assert_eq!(sanitize_rel_path("a/b/.."), Err(PathSafetyError::Traversal));
    }

    #[test]
    fn rejects_drive_letter() {
        assert_eq!(
            sanitize_rel_path("C:/Windows"),
            Err(PathSafetyError::Absolute)
        );
    }

    #[test]
    fn strips_leading_separator_to_relative() {
        // absolute-looking unix path is defanged into a safe relative path
        assert_eq!(sanitize_rel_path("/etc/passwd").unwrap(), "etc/passwd");
    }

    #[test]
    fn normalises_backslashes_and_dot_components() {
        assert_eq!(
            sanitize_rel_path("sub\\dir\\./file.txt").unwrap(),
            "sub/dir/file.txt"
        );
    }

    #[test]
    fn replaces_illegal_chars() {
        // ':' '*' '?' '|' are illegal on Windows -> become '_'
        let got = sanitize_rel_path("weird:name*?.txt").unwrap();
        assert_eq!(got, "weird_name__.txt");
    }

    #[test]
    fn strips_trailing_dot_and_space() {
        // a macOS folder name ending in '.' breaks on Windows
        assert_eq!(sanitize_rel_path("folder./file ").unwrap(), "folder/file");
    }

    #[test]
    fn escapes_reserved_windows_names() {
        assert_eq!(sanitize_rel_path("CON").unwrap(), "_CON");
        assert_eq!(sanitize_rel_path("nul.txt").unwrap(), "_nul.txt");
        assert_eq!(sanitize_rel_path("COM1").unwrap(), "_COM1");
        assert_eq!(sanitize_rel_path("LPT9.log").unwrap(), "_LPT9.log");
        // not reserved
        assert_eq!(sanitize_rel_path("COM10").unwrap(), "COM10");
        assert_eq!(sanitize_rel_path("console").unwrap(), "console");
    }

    #[test]
    fn empty_after_sanitise_is_error() {
        assert_eq!(sanitize_rel_path(""), Err(PathSafetyError::Empty));
        assert_eq!(sanitize_rel_path("/././"), Err(PathSafetyError::Empty));
    }

    #[test]
    fn too_long_is_error() {
        let long = "a/".repeat(200);
        assert!(matches!(
            sanitize_rel_path(&long),
            Err(PathSafetyError::TooLong { .. })
        ));
    }

    #[test]
    fn component_length_truncation_keeps_extension() {
        let name = format!("{}.txt", "x".repeat(300));
        let got = sanitize_component(&name).unwrap();
        assert!(got.len() <= MAX_COMPONENT_LEN);
        assert!(got.ends_with(".txt"));
    }

    #[test]
    fn collision_resolution_is_case_insensitive_and_unique() {
        let mut used = HashSet::new();
        assert_eq!(
            resolve_collision(&mut used, "Game/readme.txt"),
            "Game/readme.txt"
        );
        // same path different case collides
        assert_eq!(
            resolve_collision(&mut used, "game/README.TXT"),
            "game/README (2).TXT"
        );
        assert_eq!(
            resolve_collision(&mut used, "Game/readme.txt"),
            "Game/readme (3).txt"
        );
        // a name with no extension
        assert_eq!(resolve_collision(&mut used, "data"), "data");
        assert_eq!(resolve_collision(&mut used, "DATA"), "DATA (2)");
    }

    #[test]
    fn sanitize_component_skips_dot_and_empty() {
        assert_eq!(sanitize_component("."), None);
        assert_eq!(sanitize_component(""), None);
        // '..' fed directly has both dots trailing-stripped to empty, then
        // replaced with a single '_'. The path-level guard in
        // sanitize_rel_path is what actually blocks traversal.
        assert_eq!(sanitize_component(".."), Some("_".to_string()));
    }
}
