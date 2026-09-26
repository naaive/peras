use crate::caps::{Dir, Read};
use crate::{tool, Result};
use agent_runtime::ToolError;

const MAX_MATCHES: usize = 500;

/// Search file contents under a directory with a regular expression. Output is
/// `path:line:text` (paths relative to the directory). `glob` restricts the
/// files searched (e.g. `**/*.rs`). Respects `.gitignore`.
#[tool]
pub async fn grep(dir: Read<Dir>, pattern: String, glob: Option<String>) -> Result<String> {
    let re = regex::Regex::new(&pattern)
        .map_err(|e| ToolError::Failed(format!("invalid regex: {e}")))?;
    let files = dir.walk(glob.as_deref(), usize::MAX).await?;
    let mut out = Vec::new();
    'files: for rel in files {
        let Ok(bytes) = dir.read_bytes(&rel).await else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let line = if line.len() > 500 {
                    let mut e = 500;
                    while !line.is_char_boundary(e) {
                        e -= 1;
                    }
                    &line[..e]
                } else {
                    line
                };
                out.push(format!("{}:{}:{}", rel.display(), i + 1, line));
                if out.len() >= MAX_MATCHES {
                    out.push(format!("(results truncated at {MAX_MATCHES} matches)"));
                    break 'files;
                }
            }
        }
    }
    if out.is_empty() {
        return Ok("No matches".into());
    }
    Ok(out.join("\n"))
}
