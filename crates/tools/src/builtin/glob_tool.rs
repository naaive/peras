use crate::caps::{Dir, Read};
use crate::{tool, Result};

const MAX_FILES: usize = 1000;

/// Find files under a directory whose path (relative to the directory) matches a
/// glob pattern, e.g. `**/*.rs` or `src/*.toml`. Respects `.gitignore`.
#[tool]
pub async fn glob(dir: Read<Dir>, pattern: String) -> Result<String> {
    let files = dir.walk(Some(&pattern), MAX_FILES).await?;
    if files.is_empty() {
        return Ok("No files found".into());
    }
    let mut out: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if out.len() >= MAX_FILES {
        out.push(format!("(results truncated at {MAX_FILES} files)"));
    }
    Ok(out.join("\n"))
}
