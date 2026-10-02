//! Workspace trust as a remembered first-run decision:
//! `~/.agent/code/trusted.json` lists the trusted workspaces (a trusted
//! directory trusts everything below it).

use std::path::{Path, PathBuf};

pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    /// The store in `home`'s user directory (`None` = nothing is remembered).
    pub fn in_home(home: Option<&Path>) -> Option<TrustStore> {
        home.map(|h| TrustStore { path: h.join(".agent").join("code").join("trusted.json") })
    }

    pub fn from_env() -> Option<TrustStore> {
        TrustStore::in_home(std::env::var_os("HOME").map(PathBuf::from).as_deref())
    }

    fn load(&self) -> Vec<PathBuf> {
        std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn is_trusted(&self, dir: &Path) -> bool {
        let dir = canonical(dir);
        self.load().iter().any(|t| dir.starts_with(t))
    }

    pub fn trust(&self, dir: &Path) -> std::io::Result<()> {
        let mut all = self.load();
        let dir = canonical(dir);
        if !all.contains(&dir) {
            all.push(dir);
        }
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&all).map_err(std::io::Error::other)?)
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_covers_subdirectories() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join("sub")).unwrap();
        let s = TrustStore::in_home(Some(home.path())).unwrap();
        assert!(!s.is_trusted(ws.path()));
        s.trust(ws.path()).unwrap();
        assert!(s.is_trusted(ws.path()));
        assert!(s.is_trusted(&ws.path().join("sub")));
        assert!(!s.is_trusted(home.path()));
    }
}
