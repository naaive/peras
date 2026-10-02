//! Terminal rendering of tool calls, results, diffs, todos and usage. Pure
//! functions returning text; colors only when enabled ([`set_color`]).

use crate::tools::{Todo, TodoStatus, TodoStore};
use agent::proto::{ToolCall, ToolContent, ToolResult, Usage};
use crossterm::style::Stylize;
use std::sync::atomic::{AtomicBool, Ordering};

static COLOR: AtomicBool = AtomicBool::new(false);

pub fn set_color(on: bool) {
    COLOR.store(on, Ordering::Relaxed);
}

fn color() -> bool {
    COLOR.load(Ordering::Relaxed)
}

pub fn dim(s: &str) -> String {
    if color() { s.dark_grey().to_string() } else { s.to_string() }
}
pub fn bold(s: &str) -> String {
    if color() { s.bold().to_string() } else { s.to_string() }
}
pub fn red(s: &str) -> String {
    if color() { s.red().to_string() } else { s.to_string() }
}
pub fn green(s: &str) -> String {
    if color() { s.green().to_string() } else { s.to_string() }
}
pub fn yellow(s: &str) -> String {
    if color() { s.yellow().to_string() } else { s.to_string() }
}
pub fn cyan(s: &str) -> String {
    if color() { s.cyan().to_string() } else { s.to_string() }
}

fn short(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        return one;
    }
    format!("{}…", one.chars().take(max - 1).collect::<String>())
}

fn arg<'a>(call: &'a ToolCall, key: &str) -> Option<&'a str> {
    call.input.get(key).and_then(|v| v.as_str())
}

/// `● bash(cargo test)`: the call as a one-line title.
pub fn tool_title(call: &ToolCall) -> String {
    let detail = match call.name.as_str() {
        "bash" => arg(call, "command").map(|c| short(c, 100)),
        "read" | "write" | "edit" | "multi_edit" => arg(call, "file").map(str::to_string),
        "ls" => arg(call, "dir").map(str::to_string),
        "glob" => arg(call, "pattern").map(|p| format!("{p} in {}", arg(call, "dir").unwrap_or("."))),
        "grep" => arg(call, "pattern").map(|p| format!("\"{}\" in {}", short(p, 60), arg(call, "dir").unwrap_or("."))),
        "web_fetch" => arg(call, "url").map(str::to_string),
        "todo_write" => Some(String::new()),
        "exit_plan_mode" => Some(String::new()),
        _ => arg(call, "task").or_else(|| arg(call, "prompt")).map(|t| short(t, 80)),
    }
    .unwrap_or_else(|| short(&call.input.to_string(), 80));
    let name = bold(&call.name);
    if detail.is_empty() { format!("{} {name}", green("●")) } else { format!("{} {name}({detail})", green("●")) }
}

/// The text of a result (blobs as their preview).
pub fn result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.clone(),
            ToolContent::Blob { preview, .. } => preview.clone(),
            ToolContent::Image { .. } => "[image]".to_string(),
            ToolContent::Json { value } => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Indented preview of a result: errors in red, at most `max` lines.
pub fn result_preview(call: &ToolCall, result: &ToolResult, max: usize) -> Vec<String> {
    let text = result_text(result);
    if result.is_error {
        let first: Vec<String> = text.lines().take(max.max(1)).map(red).collect();
        return indent(first);
    }
    let lines: Vec<&str> = text.lines().collect();
    let summary = match call.name.as_str() {
        "read" => Some(format!("Read {} lines", lines.iter().filter(|l| l.contains('\t')).count())),
        "glob" | "ls" if !text.starts_with("No ") => Some(format!("{} entries", lines.len())),
        "grep" if !text.starts_with("No ") => Some(format!("{} matches", lines.iter().filter(|l| !l.starts_with('(')).count())),
        "todo_write" => None,
        "edit" | "multi_edit" | "write" => return indent(diff(call).into_iter().take(max.max(4) * 3).collect()),
        _ => None,
    };
    if let Some(s) = summary {
        return indent(vec![dim(&s)]);
    }
    if lines.is_empty() {
        return indent(vec![dim("(no output)")]);
    }
    let mut out: Vec<String> = lines.iter().take(max).map(|l| dim(&short_line(l, 160))).collect();
    if lines.len() > max {
        out.push(dim(&format!("… +{} lines", lines.len() - max)));
    }
    indent(out)
}

fn short_line(l: &str, max: usize) -> String {
    if l.chars().count() <= max {
        l.to_string()
    } else {
        format!("{}…", l.chars().take(max - 1).collect::<String>())
    }
}

fn indent(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().enumerate().map(|(i, l)| if i == 0 { format!("  ⎿  {l}") } else { format!("     {l}") }).collect()
}

/// `-`/`+` lines of an edit (`edit`, `multi_edit`) or the size of a write.
pub fn diff(call: &ToolCall) -> Vec<String> {
    let pair = |old: &str, new: &str| -> Vec<String> {
        let mut v: Vec<String> = old.lines().map(|l| red(&format!("- {l}"))).collect();
        v.extend(new.lines().map(|l| green(&format!("+ {l}"))));
        v
    };
    match call.name.as_str() {
        "edit" => pair(arg(call, "old").unwrap_or(""), arg(call, "new").unwrap_or("")),
        "multi_edit" => call
            .input
            .get("edits")
            .and_then(|e| e.as_array())
            .map(|edits| {
                edits
                    .iter()
                    .flat_map(|e| {
                        pair(e.get("old").and_then(|v| v.as_str()).unwrap_or(""), e.get("new").and_then(|v| v.as_str()).unwrap_or(""))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "write" => {
            let n = arg(call, "content").map(|c| c.lines().count()).unwrap_or(0);
            vec![green(&format!("+ {n} lines"))]
        }
        _ => vec![],
    }
}

/// The task list as a checklist.
pub fn todos(list: &[Todo]) -> Vec<String> {
    list.iter()
        .map(|t| match t.status {
            TodoStatus::Completed => dim(&format!("☒ {}", t.content)),
            TodoStatus::InProgress => bold(&format!("◐ {}", t.content)),
            TodoStatus::Pending => format!("☐ {}", t.content),
        })
        .collect()
}

/// The todos of a `todo_write` call, rendered under it.
pub fn todo_call(call: &ToolCall) -> Vec<String> {
    TodoStore::parse(&call.input).map(|t| indent(todos(&t))).unwrap_or_default()
}

/// Accumulated usage of a conversation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_micros: u64,
    pub replies: u64,
}

impl Totals {
    pub fn add(&mut self, u: &Usage) {
        self.input += u.input_tokens as u64;
        self.output += u.output_tokens as u64;
        self.cache_read += u.cache_read_tokens as u64;
        self.cache_write += u.cache_write_tokens as u64;
        self.cost_micros += u.cost_micros;
        self.replies += 1;
    }

    pub fn describe(&self) -> String {
        let cost = if self.cost_micros > 0 { format!("${:.4}", self.cost_micros as f64 / 1e6) } else { "unknown (no price table for this model)".into() };
        format!(
            "Total cost:  {cost}\nReplies:     {}\nTokens:      {} input, {} output, {} cache read, {} cache write",
            self.replies, self.input, self.output, self.cache_read, self.cache_write
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::proto::{CallId, EffectClass, Trust};
    use serde_json::json;

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall { id: CallId::new("c"), name: name.into(), input, access: vec![], class: EffectClass::Pure, isolated: false }
    }

    fn result(text: &str, is_error: bool) -> ToolResult {
        ToolResult {
            call_id: CallId::new("c"),
            content: vec![ToolContent::Text { text: text.into() }],
            is_error,
            trust: Trust::Internal,
            observed: vec![],
            staged: vec![],
            subagent: None,
            instructions: vec![],
        }
    }

    #[test]
    fn titles() {
        assert_eq!(tool_title(&call("bash", json!({"command": "cargo   test"}))), "● bash(cargo test)");
        assert_eq!(tool_title(&call("edit", json!({"file": "src/a.rs"}))), "● edit(src/a.rs)");
        assert_eq!(tool_title(&call("grep", json!({"pattern": "fn main", "dir": "src"}))), "● grep(\"fn main\" in src)");
        assert_eq!(tool_title(&call("explore", json!({"task": "find the parser"}))), "● explore(find the parser)");
        assert_eq!(tool_title(&call("todo_write", json!({"todos": []}))), "● todo_write");
    }

    #[test]
    fn previews() {
        let c = call("bash", json!({"command": "ls"}));
        let p = result_preview(&c, &result("a\nb\nc\nd", false), 2);
        assert_eq!(p, vec!["  ⎿  a", "     b", "     … +2 lines"]);
        let e = result_preview(&c, &result("boom", true), 2);
        assert_eq!(e, vec!["  ⎿  boom"]);
        let d = result_preview(&call("edit", json!({"file": "a", "old": "x", "new": "y\nz"})), &result("ok", false), 3);
        assert_eq!(d, vec!["  ⎿  - x", "     + y", "     + z"]);
    }

    #[test]
    fn todo_checklist() {
        let c = call("todo_write", json!({"todos": [
            {"content": "Read", "status": "completed"},
            {"content": "Fix", "status": "in_progress"},
            {"content": "Test", "status": "pending"}
        ]}));
        assert_eq!(todo_call(&c), vec!["  ⎿  ☒ Read", "     ◐ Fix", "     ☐ Test"]);
    }

    #[test]
    fn totals() {
        let mut t = Totals::default();
        t.add(&Usage { input_tokens: 10, output_tokens: 5, cache_read_tokens: 0, cache_write_tokens: 0, cost_micros: 1500 });
        assert!(t.describe().contains("$0.0015"));
        assert_eq!(t.replies, 1);
    }
}
