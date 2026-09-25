use crate::caps::{File, Read, Write};
use crate::{tool, Result};

const DEFAULT_LIMIT: u32 = 2000;
const MAX_LINE: usize = 2000;

/// Read a text file. Output is line-numbered (`cat -n` style). `offset` is the
/// 1-based line to start from, `limit` the number of lines (default 2000).
#[tool]
pub async fn read(file: Read<File>, offset: Option<u32>, limit: Option<u32>) -> Result<String> {
    let text = file.text().await?;
    let start = offset.unwrap_or(1).max(1) as usize;
    let limit = limit.unwrap_or(DEFAULT_LIMIT) as usize;
    let total = text.lines().count();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
        let line = if line.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}... [line truncated]", &line[..end])
        } else {
            line.to_string()
        };
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    let shown_end = (start - 1 + limit).min(total);
    if total == 0 {
        out.push_str("(empty file)\n");
    } else if start > total {
        out.push_str(&format!(
            "(offset {start} is past the end of the file: {total} lines)\n"
        ));
    } else if shown_end < total {
        out.push_str(&format!("(showing lines {start}-{shown_end} of {total})\n"));
    }
    Ok(out)
}

/// Write `content` to a file, creating it (and missing parent directories) or
/// replacing its content.
#[tool]
pub async fn write(file: Write<File>, content: String) -> Result<String> {
    file.write_text(&content).await?;
    Ok(format!("Wrote {} bytes to {}", content.len(), file.raw()))
}

/// Replace the unique occurrence of `old` with `new` in a file. Fails when `old`
/// does not occur or occurs more than once.
#[tool]
pub async fn edit(file: Write<File>, old: String, new: String) -> Result<()> {
    file.replace_once(&old, &new).await
}
