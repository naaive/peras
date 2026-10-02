//! The conversations started in a workspace (for `--continue`, `--resume`
//! and `/resume`): one JSON line per session in `.agent/code/sessions.jsonl`.
//! The journal holds the conversations themselves; this index only says
//! which of its sessions are top-level conversations and how they began.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Seconds since the Unix epoch.
    pub started: u64,
    /// The first prompt, shortened.
    pub title: String,
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(data_dir: &Path) -> History {
        History { path: data_dir.join("code").join("sessions.jsonl") }
    }

    /// Record a new conversation (once per id).
    pub fn record(&self, id: &str, first_prompt: &str) -> std::io::Result<()> {
        if self.list().iter().any(|e| e.id == id) {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let entry = Entry { id: id.to_string(), started, title: title(first_prompt) };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&entry).map_err(std::io::Error::other)?)
    }

    /// Oldest first.
    pub fn list(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return vec![] };
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    pub fn last(&self) -> Option<Entry> {
        self.list().pop()
    }

    /// An entry by id or unique id prefix.
    pub fn find(&self, id: &str) -> Option<Entry> {
        let all = self.list();
        if let Some(e) = all.iter().find(|e| e.id == id) {
            return Some(e.clone());
        }
        let mut m = all.into_iter().filter(|e| e.id.starts_with(id));
        match (m.next(), m.next()) {
            (Some(e), None) => Some(e),
            _ => None,
        }
    }
}

fn title(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 80 {
        return one_line;
    }
    format!("{}...", one_line.chars().take(77).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_lists_and_finds() {
        let d = tempfile::tempdir().unwrap();
        let h = History::new(d.path());
        assert!(h.last().is_none());
        h.record("01AAA", "fix the\n tests").unwrap();
        h.record("01AAB", &"x".repeat(200)).unwrap();
        h.record("01AAA", "again").unwrap();
        let all = h.list();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].title, "fix the tests");
        assert!(all[1].title.ends_with("...") && all[1].title.chars().count() == 80);
        assert_eq!(h.last().unwrap().id, "01AAB");
        assert_eq!(h.find("01AAB").unwrap().id, "01AAB");
        assert!(h.find("01AA").is_none(), "ambiguous prefix");
    }
}
