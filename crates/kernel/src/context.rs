//! The model context: a projection of the journal that is append-only within a
//! request sequence. Replacement events are the only way to shorten it.

use crate::render::{self, RuleSet};
use crate::state::State;
use agent_proto::*;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EntryKind {
    User,
    Assistant,
    ToolResult,
    Snapshot { key: String },
    Summary,
    Other,
}

/// One rendered fragment of the context.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub seq: Seq,
    pub id: EventId,
    pub kind: EntryKind,
    pub rendered: Arc<Rendered>,
    /// Untrusted source labels of the content.
    pub untrusted: Vec<String>,
    /// Original event and trust (for re-rendering on model switch). `None` once
    /// the entry was trimmed or replaced.
    pub source: Option<Arc<(Event, Trust)>>,
    pub trimmed: bool,
    pub at: Timestamp,
}

/// Context operations in application order (used to re-project after a rewind).
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Append(Entry),
    Replace { seq: Seq, id: EventId, at: Timestamp, rep: Arc<Replacement> },
}

impl Op {
    pub fn seq(&self) -> Seq {
        match self {
            Op::Append(e) => e.seq,
            Op::Replace { seq, .. } => *seq,
        }
    }
}

pub(crate) fn apply_replacement(ctx: &mut Vec<Entry>, id: &EventId, at: Timestamp, rep: &Replacement) {
    let (a, b) = rep.range;
    let idxs: Vec<usize> = ctx.iter().enumerate().filter(|(_, e)| e.seq >= a && e.seq <= b).map(|(i, _)| i).collect();
    let Some(&first) = idxs.first() else { return };
    let removed: Vec<Entry> = idxs.iter().rev().map(|&i| ctx.remove(i)).collect::<Vec<_>>().into_iter().rev().collect();
    let one_to_one = matches!(rep.kind, ReplacementKind::Rerender | ReplacementKind::Trim | ReplacementKind::ImageOffload)
        && rep.content.len() == removed.len();
    let new: Vec<Entry> = if one_to_one {
        removed
            .into_iter()
            .zip(rep.content.iter())
            .map(|(e, r)| Entry {
                rendered: Arc::new(r.clone()),
                source: if rep.kind == ReplacementKind::Rerender { e.source } else { None },
                trimmed: e.trimmed || rep.kind != ReplacementKind::Rerender,
                ..e
            })
            .collect()
    } else {
        rep.content
            .iter()
            .map(|r| Entry {
                seq: a,
                id: id.clone(),
                kind: if rep.kind == ReplacementKind::Summary { EntryKind::Summary } else { EntryKind::Other },
                rendered: Arc::new(r.clone()),
                untrusted: rep.untrusted_sources.clone(),
                source: None,
                trimmed: true,
                at,
            })
            .collect()
    };
    let at_pos = first.min(ctx.len());
    for (k, e) in new.into_iter().enumerate() {
        ctx.insert(at_pos + k, e);
    }
}

/// Group boundaries: a tool result always belongs to the preceding group, so a
/// tool_use is never separated from its result. Returns [start, end) ranges.
pub(crate) fn groups(ctx: &[Entry]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (i, e) in ctx.iter().enumerate() {
        if e.kind == EntryKind::ToolResult && !out.is_empty() {
            out.last_mut().unwrap().1 = i + 1;
        } else {
            out.push((i, i + 1));
        }
    }
    out
}

fn tokens(ctx: &[Entry], g: (usize, usize)) -> u32 {
    ctx[g.0..g.1].iter().map(|e| e.rendered.tokens).sum()
}

pub(crate) fn head_tokens(head: &SeqHead) -> u32 {
    let sys: u32 = head.system.iter().map(|s| estimate_tokens(s)).sum();
    let tools: u32 = head
        .tools
        .iter()
        .map(|t| estimate_tokens(&serde_json::to_string(t).unwrap_or_default()))
        .sum();
    sys + tools
}

pub(crate) fn estimated_total(s: &State) -> u32 {
    let h = s.head.as_ref().map(head_tokens).unwrap_or(0);
    h + s.context.iter().map(|e| e.rendered.tokens).sum::<u32>()
}

/// Last actual usage + estimate of what was appended since (or a full estimate
/// after a replacement / new sequence).
pub(crate) fn usage(s: &State) -> u32 {
    match s.usage_basis {
        Some(b) => b.saturating_add(s.est_since),
        None => estimated_total(s),
    }
}

pub(crate) fn pressure_limit(s: &State) -> u32 {
    let (Some(cfg), Some(caps)) = (s.config.as_ref(), s.caps.as_ref()) else { return u32::MAX };
    let w = caps.window as f64 * cfg.compaction.pressure_ratio as f64;
    (w as u64).saturating_sub(cfg.compaction.output_reserve as u64).min(u32::MAX as u64) as u32
}

pub(crate) fn hard_limit(s: &State) -> u32 {
    let (Some(cfg), Some(caps)) = (s.config.as_ref(), s.caps.as_ref()) else { return u32::MAX };
    caps.window.saturating_sub(cfg.compaction.output_reserve)
}

pub(crate) fn under_pressure(s: &State) -> bool {
    usage(s) > pressure_limit(s)
}

/// Index of the first entry of the verbatim tail (groups from the end whose
/// total stays within `keep` tokens; the last group is always kept).
fn tail_start(ctx: &[Entry], keep: u32) -> usize {
    let gs = groups(ctx);
    let Some(last) = gs.last() else { return 0 };
    let mut start = last.0;
    let mut total = tokens(ctx, *last);
    for g in gs.iter().rev().skip(1) {
        let t = tokens(ctx, *g);
        if total + t > keep {
            break;
        }
        total += t;
        start = g.0;
    }
    start
}

fn keep_recent(s: &State) -> u32 {
    s.config.as_ref().map(|c| c.compaction.keep_recent_tokens).unwrap_or(0)
}

fn profile(s: &State) -> RenderProfile {
    s.head.as_ref().map(|h| h.render.clone()).unwrap_or_default()
}

/// Levels 2 and 3: trims of old tool results, removal of superseded snapshots,
/// image offloading. `forced` (overflow path) applies them to everything but the
/// last group.
pub(crate) fn plan_trims(s: &State, forced: bool) -> Vec<Replacement> {
    let rules = RuleSet::default();
    let ctx = &s.context;
    let end = if forced {
        groups(ctx).last().map(|g| g.0).unwrap_or(0)
    } else {
        tail_start(ctx, keep_recent(s))
    };
    let keep = profile(s).preview_bytes as usize;
    let mut out = Vec::new();
    for (i, e) in ctx.iter().enumerate().take(end) {
        let rep = |kind, content: Vec<Rendered>| Replacement {
            kind,
            range: (e.seq, e.seq),
            sources: vec![e.id.clone()],
            untrusted_sources: e.untrusted.clone(),
            content,
        };
        // Replacement fragments share a seq; only touch entries that own theirs.
        if ctx.iter().filter(|x| x.seq == e.seq).count() != 1 {
            continue;
        }
        match &e.kind {
            EntryKind::Snapshot { key } => {
                let superseded = ctx[i + 1..].iter().any(|x| matches!(&x.kind, EntryKind::Snapshot { key: k } if k == key));
                if superseded {
                    out.push(rep(ReplacementKind::Trim, vec![]));
                }
            }
            EntryKind::ToolResult if !e.trimmed => {
                let trimmed = render::trim(&rules, &e.rendered, keep);
                let base = trimmed.clone().unwrap_or_else(|| (*e.rendered).clone());
                let off = render::offload_images(&rules, &base);
                match (trimmed, off) {
                    (_, Some(o)) => out.push(rep(ReplacementKind::ImageOffload, vec![o])),
                    (Some(t), None) if t.tokens < e.rendered.tokens => out.push(rep(ReplacementKind::Trim, vec![t])),
                    _ => {}
                }
            }
            EntryKind::User => {
                if let Some(o) = render::offload_images(&rules, &e.rendered) {
                    out.push(rep(ReplacementKind::ImageOffload, vec![o]));
                }
            }
            _ => {}
        }
    }
    out
}

/// A range of whole groups to summarise.
#[derive(Debug, Clone)]
pub(crate) struct SummaryPlan {
    pub range: (Seq, Seq),
    pub body: Vec<Rendered>,
}

fn plan_from(ctx: &[Entry], upto: usize) -> Option<SummaryPlan> {
    if upto == 0 {
        return None;
    }
    let part = &ctx[..upto];
    // Nothing to gain from summarising a lone summary.
    if part.len() == 1 && part[0].kind == EntryKind::Summary {
        return None;
    }
    let lo = part.iter().map(|e| e.seq).min()?;
    let hi = part.iter().map(|e| e.seq).max()?;
    Some(SummaryPlan { range: (lo, hi), body: part.iter().map(|e| (*e.rendered).clone()).collect() })
}

/// Pressure path: everything before the verbatim tail.
pub(crate) fn plan_summary(s: &State) -> Option<SummaryPlan> {
    plan_from(&s.context, tail_start(&s.context, keep_recent(s)))
}

/// Overflow path: only the earliest segment (about half a window), never the last group.
pub(crate) fn plan_overflow_segment(s: &State) -> Option<SummaryPlan> {
    let ctx = &s.context;
    let gs = groups(ctx);
    if gs.len() < 2 {
        return None;
    }
    let budget = s.caps.as_ref().map(|c| c.window / 2).unwrap_or(u32::MAX);
    let mut upto = 0;
    let mut total = 0u32;
    for g in &gs[..gs.len() - 1] {
        let t = tokens(ctx, *g);
        if upto > 0 && total + t > budget {
            break;
        }
        total += t;
        upto = g.1;
    }
    plan_from(ctx, upto)
}

pub(crate) fn entries_in(s: &State, range: (Seq, Seq)) -> Vec<&Entry> {
    s.context.iter().filter(|e| e.seq >= range.0 && e.seq <= range.1).collect()
}

/// Model switch: re-render the whole history with the (new) head's profile.
pub(crate) fn rerender(s: &State) -> Option<Replacement> {
    let rules = RuleSet::default();
    let prof = profile(s);
    let first = s.context.first()?;
    let last = s.context.last()?;
    let lo = s.context.iter().map(|e| e.seq).min().unwrap_or(first.seq);
    let hi = s.context.iter().map(|e| e.seq).max().unwrap_or(last.seq);
    let mut sources = Vec::new();
    let mut labels = Vec::new();
    let content = s
        .context
        .iter()
        .map(|e| {
            if !sources.contains(&e.id) {
                sources.push(e.id.clone());
            }
            for l in &e.untrusted {
                if !labels.contains(l) {
                    labels.push(l.clone());
                }
            }
            let fresh = e.source.as_ref().and_then(|b| render::render(&rules, &prof, &b.1, &b.0));
            render::strip_vendor(&rules, fresh.as_ref().unwrap_or(&e.rendered))
        })
        .collect();
    Some(Replacement { kind: ReplacementKind::Rerender, range: (lo, hi), sources, untrusted_sources: labels, content })
}

/// Every tool_use has exactly one later tool_result and vice versa.
pub(crate) fn well_paired(ctx: &[Entry]) -> bool {
    let mut open: Vec<&CallId> = Vec::new();
    for e in ctx {
        for b in &e.rendered.blocks {
            match b {
                RBlock::ToolUse { id, .. } => open.push(id),
                RBlock::ToolResult { id, .. } => match open.iter().position(|o| *o == id) {
                    Some(i) => {
                        open.remove(i);
                    }
                    None => return false,
                },
                _ => {}
            }
        }
    }
    open.is_empty()
}

pub(crate) fn source_of(e: &Entry) -> crate::ContextSource {
    let base = match &e.kind {
        EntryKind::User => "user_message",
        EntryKind::Assistant => "assistant_replied",
        EntryKind::ToolResult => "tool_resulted",
        EntryKind::Snapshot { .. } => "state_snapshot",
        EntryKind::Summary => "replaced:summary",
        EntryKind::Other => "replaced",
    };
    let kind = match (&e.source, &e.kind) {
        (Some(src), _) => src.0.type_name().to_string(),
        (None, EntryKind::Summary) => base.to_string(),
        (None, _) if e.trimmed => {
            let offloaded = e.rendered.blocks.iter().any(|b| match b {
                RBlock::Text { text } => text.starts_with("[image offloaded:"),
                RBlock::ToolResult { content, .. } => content
                    .iter()
                    .any(|c| matches!(c, RBlock::Text { text } if text.starts_with("[image offloaded:"))),
                _ => false,
            });
            format!("{base}:{}", if offloaded { "images_offloaded" } else { "trimmed" })
        }
        (None, _) => base.to_string(),
    };
    crate::ContextSource { seq: e.seq, event: e.id.clone(), kind, rendered: (*e.rendered).clone() }
}
