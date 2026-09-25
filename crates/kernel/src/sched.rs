//! Batch scheduling: a read/write lock conflict graph over declared accesses.

use agent_proto::*;

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

fn literal_prefix(s: &str) -> &str {
    match s.find(['*', '?', '[', '{']) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Could the two resource URIs (possibly globs) denote overlapping resources?
/// Conservative: glob patterns overlap when one literal prefix contains the other.
pub fn overlaps(a: &ResourceUri, b: &ResourceUri) -> bool {
    if a == b {
        return true;
    }
    if a.scheme() != b.scheme() {
        return false;
    }
    let (x, y) = (a.as_str(), b.as_str());
    if !has_glob(x) && !has_glob(y) {
        if a.scheme() == Some(Scheme::Fs) {
            let (x, y) = (x.trim_end_matches('/'), y.trim_end_matches('/'));
            return x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/"));
        }
        return false;
    }
    let (px, py) = (literal_prefix(x), literal_prefix(y));
    px.starts_with(py) || py.starts_with(px)
}

/// Two calls conflict when either is Opaque (exclusive over the workspace), when
/// they touch overlapping resources and at least one side writes, or when both are
/// side-effecting without any declaration to reason about.
pub fn conflicts(a: &ToolCall, b: &ToolCall) -> bool {
    if a.class == EffectClass::Opaque || b.class == EffectClass::Opaque {
        return true;
    }
    let se = |c: &ToolCall| c.class != EffectClass::Pure || c.access.iter().any(|x| x.mode == AccessMode::Write);
    if se(a) && se(b) && (a.access.is_empty() || b.access.is_empty()) {
        return true;
    }
    a.access.iter().any(|x| {
        b.access.iter().any(|y| {
            (x.mode == AccessMode::Write || y.mode == AccessMode::Write) && overlaps(&x.resource, &y.resource)
        })
    })
}

/// Greedy next batch over `calls` (in model order): a call joins when it
/// conflicts neither with a chosen call nor with an earlier skipped one (so
/// conflicting calls keep their relative order). Returns indices.
pub fn next_batch(calls: &[&ToolCall]) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::new();
    let mut skipped: Vec<usize> = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        let blocked = chosen.iter().chain(skipped.iter()).any(|&j| conflicts(c, calls[j]));
        if blocked {
            skipped.push(i);
        } else {
            chosen.push(i);
        }
    }
    chosen
}

/// Does executing this batch write anything (checkpoint first)?
pub fn batch_writes(calls: &[&ToolCall]) -> Vec<Access> {
    let mut out = Vec::new();
    for c in calls {
        for a in &c.access {
            if a.mode == AccessMode::Write && !out.contains(a) {
                out.push(a.clone());
            }
        }
    }
    out
}

pub fn needs_checkpoint(calls: &[&ToolCall]) -> bool {
    calls.iter().any(|c| {
        matches!(c.class, EffectClass::LocalWrite | EffectClass::Opaque)
            || c.access.iter().any(|a| a.mode == AccessMode::Write)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, class: EffectClass, access: Vec<Access>) -> ToolCall {
        ToolCall { id: id.into(), name: "t".into(), input: serde_json::json!({}), access, class }
    }

    #[test]
    fn reads_parallel_writes_serialise() {
        let r1 = call("1", EffectClass::Pure, vec![Access::read(ResourceUri::fs("/w/a"))]);
        let r2 = call("2", EffectClass::Pure, vec![Access::read(ResourceUri::fs("/w/a"))]);
        let w = call("3", EffectClass::LocalWrite, vec![Access::write(ResourceUri::fs("/w/a"))]);
        let r3 = call("4", EffectClass::Pure, vec![Access::read(ResourceUri::fs("/w/b"))]);
        let calls = [&r1, &r2, &w, &r3];
        assert_eq!(next_batch(&calls), vec![0, 1, 3]);
        let rest = [&w];
        assert_eq!(next_batch(&rest), vec![0]);
    }

    #[test]
    fn glob_overlap_and_opaque() {
        assert!(overlaps(&ResourceUri("fs:///w/**".into()), &ResourceUri::fs("/w/src/a.rs")));
        assert!(!overlaps(&ResourceUri("fs:///w/src/**".into()), &ResourceUri::fs("/w/doc/a.md")));
        assert!(!overlaps(&ResourceUri::fs("/w/a"), &ResourceUri::net("a", 1)));
        let o = call("o", EffectClass::Opaque, vec![]);
        let r = call("r", EffectClass::Pure, vec![]);
        assert!(conflicts(&o, &r));
        let calls = [&r, &o, &r];
        assert_eq!(next_batch(&calls), vec![0]);
    }
}
