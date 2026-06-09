//! Data-root resolution and the per-component directory tree (TDD §17).
//!
//! A single configurable data root holds everything the app writes; binaries
//! live elsewhere (Program Files / Applications) and are replaced wholesale by
//! the updater — **the updater must never touch the data root** (HARD
//! CONSTRAINT #14). Server and client are separate binaries with separate state
//! under `server/` and `client/` subfolders of the one root.
//!
//! Defaults: Windows `%LOCALAPPDATA%\BLT\`, macOS
//! `~/Library/Application Support/BLT/`. An explicit override relocates it.

use std::io;
use std::path::{Path, PathBuf};

/// The resolved data root (`…/BLT`).
#[derive(Debug, Clone)]
pub struct DataRoot {
    root: PathBuf,
}

/// Which binary's subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Server,
    Client,
}

impl Component {
    fn dir_name(self) -> &'static str {
        match self {
            Component::Server => "server",
            Component::Client => "client",
        }
    }
    /// The SQLite filename for this component.
    pub fn db_file(self) -> &'static str {
        match self {
            Component::Server => "blt-server.db",
            Component::Client => "blt-client.db",
        }
    }
}

impl DataRoot {
    /// Resolve the data root: explicit `override_root` if given, else the per-OS
    /// default. Does **not** create anything — call [`Component`]-specific
    /// [`DataRoot::component_dirs`] + [`ComponentDirs::ensure`] for that.
    pub fn resolve(override_root: Option<PathBuf>) -> io::Result<Self> {
        let root = match override_root {
            Some(p) => p,
            None => default_data_root()?,
        };
        Ok(DataRoot { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The directory layout for one component (does not create it).
    pub fn component_dirs(&self, c: Component) -> ComponentDirs {
        let base = self.root.join(c.dir_name());
        ComponentDirs {
            config_path: base.join("config.toml"),
            db_path: base.join(c.db_file()),
            backups: base.join("backups"),
            cache: base.join("cache"),
            logs: base.join("logs"),
            base,
        }
    }
}

/// The concrete paths under one component subfolder (TDD §17.2).
#[derive(Debug, Clone)]
pub struct ComponentDirs {
    pub base: PathBuf,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub backups: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
}

impl ComponentDirs {
    /// Create the subfolder tree if missing (idempotent). Backups exists only on
    /// the server in practice but creating it on both is harmless.
    pub fn ensure(&self) -> io::Result<()> {
        for d in [&self.base, &self.backups, &self.cache, &self.logs] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

/// The per-OS default data root.
pub fn default_data_root() -> io::Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(local).join("BLT"));
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "LOCALAPPDATA not set",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let home = home_dir()?;
        Ok(home.join("Library/Application Support/BLT"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // Linux is not a supported target, but keep dev builds working.
        let home = home_dir()?;
        Ok(home.join(".local/share/BLT"))
    }
}

#[allow(dead_code)]
fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_root_layout() {
        let dir = tempfile::tempdir().unwrap();
        let dr = DataRoot::resolve(Some(dir.path().join("BLT"))).unwrap();
        let server = dr.component_dirs(Component::Server);
        assert!(server.base.ends_with("BLT/server"));
        assert!(server.db_path.ends_with("blt-server.db"));
        assert!(server.logs.ends_with("server/logs"));

        server.ensure().unwrap();
        assert!(server.logs.is_dir());
        assert!(server.cache.is_dir());
        assert!(server.backups.is_dir());

        let client = dr.component_dirs(Component::Client);
        assert!(client.db_path.ends_with("blt-client.db"));
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let dr = DataRoot::resolve(Some(dir.path().to_path_buf())).unwrap();
        let c = dr.component_dirs(Component::Client);
        c.ensure().unwrap();
        c.ensure().unwrap(); // no error second time
    }
}
