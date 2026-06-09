//! Free-space pre-flight (F4.4/F6.6, HARD CONSTRAINT #6): warn + confirm when
//! the destination volume lacks space — never hard-block.

use std::path::Path;

/// Available bytes on the volume containing `path` (climbs to the nearest
/// existing ancestor so a not-yet-created destination folder still works).
pub fn available_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    loop {
        if p.exists() {
            return fs4::available_space(p).ok();
        }
        p = p.parent()?;
    }
}

/// `(enough, available)` for a payload of `needed` bytes at `dest`.
pub fn preflight(dest: &Path, needed: u64) -> (bool, Option<u64>) {
    match available_bytes(dest) {
        Some(avail) => (avail >= needed, Some(avail)),
        None => (true, None), // unknowable → don't scare the user
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_on_real_volume() {
        let tmp = std::env::temp_dir();
        let (enough_small, avail) = preflight(&tmp, 1);
        assert!(enough_small);
        assert!(avail.is_some());
        // an absurd requirement fails the check but still reports
        let (enough_huge, _) = preflight(&tmp, u64::MAX);
        assert!(!enough_huge);
    }

    #[test]
    fn preflight_nonexistent_dest_climbs() {
        let dest = std::env::temp_dir().join("blt-does-not-exist/sub/dir");
        let (_, avail) = preflight(&dest, 1);
        assert!(avail.is_some(), "climbs to existing ancestor");
    }
}
