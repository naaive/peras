//! Minimal frontmatter parser for skill / command / agent Markdown files.
//!
//! Supports `key: value`, `key: [a, b]`, and YAML block lists
//! (`key:` followed by `  - a` lines). Quotes around scalars are stripped.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FmValue {
    Str(String),
    List(Vec<String>),
}

impl FmValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FmValue::Str(s) => Some(s),
            FmValue::List(_) => None,
        }
    }
    /// Lists as-is; a scalar is split on commas.
    pub fn as_list(&self) -> Vec<String> {
        match self {
            FmValue::List(l) => l.clone(),
            FmValue::Str(s) => s
                .split(',')
                .map(|x| unquote(x.trim()).to_string())
                .filter(|x| !x.is_empty())
                .collect(),
        }
    }
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Split a document into (frontmatter fields, body). No frontmatter = empty map.
pub fn parse(text: &str) -> (BTreeMap<String, FmValue>, String) {
    let mut fields = BTreeMap::new();
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = t.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (fields, text.to_string());
    }
    let mut fm: Vec<&str> = vec![];
    let mut closed = false;
    for l in lines.by_ref() {
        if l.trim_end() == "---" {
            closed = true;
            break;
        }
        fm.push(l);
    }
    if !closed {
        return (BTreeMap::new(), text.to_string());
    }
    let body: Vec<&str> = lines.collect();
    let mut body = body.join("\n");
    if t.ends_with('\n') && !body.is_empty() {
        body.push('\n');
    }
    let body = body.trim_start_matches('\n').to_string();

    let mut current_list: Option<String> = None;
    for l in fm {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(k) = &current_list {
                if let Some(FmValue::List(v)) = fields.get_mut(k) {
                    v.push(unquote(item).to_string());
                }
            }
            continue;
        }
        current_list = None;
        let Some((k, v)) = trimmed.split_once(':') else { continue };
        let k = k.trim().to_string();
        let v = v.trim();
        if v.is_empty() {
            fields.insert(k.clone(), FmValue::List(vec![]));
            current_list = Some(k);
        } else if v.starts_with('[') && v.ends_with(']') {
            let inner = &v[1..v.len() - 1];
            let items = inner
                .split(',')
                .map(|x| unquote(x).to_string())
                .filter(|x| !x.is_empty())
                .collect();
            fields.insert(k, FmValue::List(items));
        } else {
            fields.insert(k, FmValue::Str(unquote(v).to_string()));
        }
    }
    (fields, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_and_body() {
        let (f, b) = parse("---\nname: \"rev\"\ntools: [read, grep]\nmodel: m\n---\nBody here\n");
        assert_eq!(f["name"], FmValue::Str("rev".into()));
        assert_eq!(f["tools"].as_list(), vec!["read", "grep"]);
        assert_eq!(b, "Body here\n");
    }

    #[test]
    fn block_lists_and_comma_scalars() {
        let (f, _) = parse("---\ntools:\n  - read\n  - 'edit'\nother: a, b\n---\n");
        assert_eq!(f["tools"].as_list(), vec!["read", "edit"]);
        assert_eq!(f["other"].as_list(), vec!["a", "b"]);
    }

    #[test]
    fn no_frontmatter() {
        let (f, b) = parse("# Title\ntext");
        assert!(f.is_empty());
        assert_eq!(b, "# Title\ntext");
        let (f, _) = parse("---\nunterminated: x\n");
        assert!(f.is_empty());
    }
}
