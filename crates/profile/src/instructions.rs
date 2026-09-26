//! Instruction files (AGENTS.md / CLAUDE.md) and their byte budget.

use serde::{Deserialize, Serialize};

/// Default byte budget for all instruction files together.
pub const DEFAULT_INSTRUCTION_BUDGET: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionFile {
    pub path: String,
    pub text: String,
    /// Trusted guidance (Static layer) vs untrusted data.
    pub trusted: bool,
    /// The text was cut to fit the budget.
    #[serde(default)]
    pub truncated: bool,
}

/// Fit `files` (ordered broadest first: user file, project root ... cwd) into
/// `max_bytes`. Policy: omit the broadest files first; if the most specific file
/// alone still exceeds the budget, truncate it (at a char boundary).
///
/// Guarantees: the total size of the result is `<= max_bytes`; the result is a
/// suffix of the input; only the first remaining file can be truncated, and
/// only when it is the sole survivor. Returns human-readable notes.
pub fn apply_budget(files: Vec<InstructionFile>, max_bytes: usize) -> (Vec<InstructionFile>, Vec<String>) {
    let mut notes = vec![];
    let mut total: usize = files.iter().map(|f| f.text.len()).sum();
    let mut files: std::collections::VecDeque<InstructionFile> = files.into();
    while total > max_bytes && files.len() > 1 {
        let f = files.pop_front().expect("len > 1");
        total -= f.text.len();
        notes.push(format!(
            "instructions: omitted {} ({} bytes) to fit the {} byte budget",
            f.path,
            f.text.len(),
            max_bytes
        ));
    }
    if total > max_bytes {
        if let Some(mut f) = files.pop_front() {
            let mut cut = max_bytes.min(f.text.len());
            while !f.text.is_char_boundary(cut) {
                cut -= 1;
            }
            notes.push(format!(
                "instructions: truncated {} from {} to {} bytes",
                f.path,
                f.text.len(),
                cut
            ));
            f.text.truncate(cut);
            f.truncated = true;
            if !f.text.is_empty() {
                files.push_front(f);
            }
        }
    }
    (files.into(), notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn f(p: &str, n: usize) -> InstructionFile {
        InstructionFile { path: p.into(), text: "x".repeat(n), trusted: true, truncated: false }
    }

    #[test]
    fn fits_untouched() {
        let (out, notes) = apply_budget(vec![f("a", 3), f("b", 4)], 7);
        assert_eq!(out.len(), 2);
        assert!(notes.is_empty());
    }

    #[test]
    fn omits_broader_first() {
        let (out, notes) = apply_budget(vec![f("root", 5), f("mid", 5), f("cwd", 5)], 11);
        assert_eq!(out.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(), vec!["mid", "cwd"]);
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn truncates_most_specific_last() {
        let (out, _) = apply_budget(vec![f("root", 5), f("cwd", 10)], 4);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "cwd");
        assert_eq!(out[0].text.len(), 4);
        assert!(out[0].truncated);
    }

    #[test]
    fn truncates_on_char_boundary() {
        let file = InstructionFile { path: "c".into(), text: "€€€".into(), trusted: true, truncated: false };
        let (out, _) = apply_budget(vec![file], 4);
        assert_eq!(out[0].text, "€");
    }

    proptest! {
        #[test]
        fn budget_properties(sizes in proptest::collection::vec(0usize..50, 0..6), max in 0usize..120) {
            let files: Vec<_> = sizes.iter().enumerate().map(|(i, n)| f(&format!("f{i}"), *n)).collect();
            let (out, _) = apply_budget(files.clone(), max);
            let total: usize = out.iter().map(|x| x.text.len()).sum();
            prop_assert!(total <= max);
            // result paths are a suffix of the input paths
            let in_paths: Vec<_> = files.iter().map(|x| x.path.clone()).collect();
            let out_paths: Vec<_> = out.iter().map(|x| x.path.clone()).collect();
            prop_assert!(in_paths.ends_with(&out_paths));
            // only the first survivor may be truncated, and only when it is alone
            for (i, x) in out.iter().enumerate() {
                if x.truncated { prop_assert!(i == 0 && out.len() == 1); }
            }
            let in_total: usize = sizes.iter().sum();
            if in_total <= max { prop_assert_eq!(out.len(), files.len()); }
        }
    }
}
