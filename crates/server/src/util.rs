//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open a file read-only, allowing it to be **deleted while open**. On Windows
/// the default share mode blocks deleting an open file, so a share deleted while
/// it's being streamed leaves orphaned bytes; adding `FILE_SHARE_DELETE` makes it
/// behave like Unix unlink-while-open (the file is removed once the stream's
/// handle closes). Part of the delete-race fix; the orphan sweep is the backstop.
pub fn open_read_shared(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path)
    }
}
