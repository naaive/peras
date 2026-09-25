//! Write-time rendering. `render` sees exactly one event: no other events, no state.
//!
//! Presentation is decided by the event's trust label:
//! - `Trust::User` → verbatim text;
//! - `Trust::Guidance` → [`RBlock::Guidance`] (the encoder decides between a
//!   mid-sequence system message and a `<system-reminder>` wrapper);
//! - `Trust::Untrusted` → [`RBlock::Data`] carrying the source label, with the
//!   profile's fixed `data_warning` prepended to the text.

use agent_proto::*;

/// Presentation rules that are not part of the model's render profile.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleSet {
    /// Token estimate charged for one image block.
    pub image_tokens: u32,
}

impl Default for RuleSet {
    fn default() -> Self {
        RuleSet { image_tokens: 1_000 }
    }
}

/// Called once when an event is written; the result is stored with the event and
/// projected verbatim afterwards. `None` for events that are not model-visible.
pub fn render(rules: &RuleSet, profile: &RenderProfile, trust: &Trust, ev: &Event) -> Option<Rendered> {
    let (role, blocks, supersedable) = match ev {
        Event::UserMessage { text, attachments } => {
            let mut blocks = vec![frame(profile, trust, text.clone())];
            for a in attachments {
                let t = match &a.content {
                    Some(c) => format!("<attachment path=\"{}\">\n{}\n</attachment>", a.path, clip(profile, c)),
                    None => format!("<attachment path=\"{}\"/>", a.path),
                };
                blocks.push(frame(profile, trust, t));
            }
            (Role::User, blocks, false)
        }
        Event::Injected { source, text } => {
            let t = match trust {
                Trust::User => text.clone(),
                _ => format!("[{source}] {text}"),
            };
            (Role::User, vec![frame(profile, trust, t)], false)
        }
        Event::AssistantReplied { message, .. } => {
            let blocks = message
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => RBlock::Text { text: text.clone() },
                    ContentBlock::Thinking { text, signature } => {
                        RBlock::Thinking { text: text.clone(), signature: signature.clone() }
                    }
                    ContentBlock::ToolUse(c) => {
                        RBlock::ToolUse { id: c.id.clone(), name: c.name.clone(), input: c.input.clone() }
                    }
                    ContentBlock::Opaque { vendor, data } => {
                        RBlock::Opaque { vendor: vendor.clone(), data: data.clone() }
                    }
                })
                .collect();
            (Role::Assistant, blocks, false)
        }
        Event::ToolResulted { call, result } => {
            let mut content = Vec::new();
            for c in &result.content {
                match c {
                    ToolContent::Text { text } => content.push(tool_frame(profile, trust, clip(profile, text))),
                    ToolContent::Blob { blob, preview } => content.push(tool_frame(
                        profile,
                        trust,
                        format!("{preview}\n[full output stored as blob sha256:{} ({} bytes)]", blob.sha256, blob.size),
                    )),
                    ToolContent::Image { blob } => content.push(RBlock::Image { blob: blob.clone() }),
                    ToolContent::Json { value } => {
                        let s = serde_json::to_string(value).unwrap_or_default();
                        content.push(tool_frame(profile, trust, clip(profile, &s)))
                    }
                }
            }
            let _ = call;
            (
                Role::User,
                vec![RBlock::ToolResult { id: result.call_id.clone(), content, is_error: result.is_error }],
                false,
            )
        }
        Event::StateSnapshot { key, text } => {
            (Role::User, vec![frame(profile, trust, format!("[state:{key}] {text}"))], true)
        }
        Event::SnapshotCleared { key } => {
            (Role::User, vec![frame(profile, trust, format!("[state:{key}] (cleared; earlier values are void)"))], true)
        }
        Event::InstructionsInjected { path, text } => {
            (Role::User, vec![frame(profile, trust, format!("Instructions from {path}:\n{text}"))], false)
        }
        Event::MemoryLoaded { text } => (Role::User, vec![frame(profile, trust, text.clone())], false),
        _ => return None,
    };
    let tokens = estimate_blocks(rules, &blocks);
    Some(Rendered { role, blocks, tokens, supersedable })
}

/// Frame a piece of text according to its trust label.
pub fn frame(profile: &RenderProfile, trust: &Trust, text: String) -> RBlock {
    match trust {
        Trust::User | Trust::Internal => RBlock::Text { text },
        Trust::Guidance => RBlock::Guidance { text },
        Trust::Untrusted { source } => {
            RBlock::Data { source: source.clone(), text: format!("{}\n\n{}", profile.data_warning, text) }
        }
    }
}

/// Tool results: trusted content is plain text; untrusted content is data-framed.
fn tool_frame(profile: &RenderProfile, trust: &Trust, text: String) -> RBlock {
    match trust {
        Trust::Untrusted { .. } => frame(profile, trust, text),
        _ => RBlock::Text { text },
    }
}

/// Content above the inline limit should already have been spilled by the
/// runtime; if not, keep head + tail so the rendering stays bounded.
fn clip(profile: &RenderProfile, text: &str) -> String {
    if text.len() as u64 <= profile.inline_limit_bytes as u64 {
        text.to_string()
    } else {
        preview(text, profile.preview_bytes as usize)
    }
}

/// Head + tail preview of `text` with `keep` bytes on each side (char-boundary safe).
pub fn preview(text: &str, keep: usize) -> String {
    if text.len() <= keep.saturating_mul(2) {
        return text.to_string();
    }
    let mut h = keep;
    while !text.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = text.len() - keep;
    while !text.is_char_boundary(t) {
        t += 1;
    }
    let omitted = t - h;
    format!("{}\n[... {omitted} bytes omitted ...]\n{}", &text[..h], &text[t..])
}

/// Deterministic token estimate of a list of blocks.
pub fn estimate_blocks(rules: &RuleSet, blocks: &[RBlock]) -> u32 {
    blocks.iter().map(|b| estimate_block(rules, b)).sum()
}

fn estimate_block(rules: &RuleSet, b: &RBlock) -> u32 {
    match b {
        RBlock::Text { text } | RBlock::Guidance { text } => estimate_tokens(text),
        RBlock::Data { source, text } => estimate_tokens(source) + estimate_tokens(text),
        RBlock::Image { .. } => rules.image_tokens,
        RBlock::ToolUse { name, input, .. } => {
            estimate_tokens(name) + estimate_tokens(&serde_json::to_string(input).unwrap_or_default())
        }
        RBlock::ToolResult { content, .. } => 1 + estimate_blocks(rules, content),
        RBlock::Thinking { text, signature } => {
            estimate_tokens(text) + signature.as_deref().map(estimate_tokens).unwrap_or(0)
        }
        RBlock::Opaque { data, .. } => estimate_tokens(&serde_json::to_string(data).unwrap_or_default()),
    }
}

/// Drop vendor-private content (opaque blocks, thinking and its signature):
/// used when history is re-rendered for a different model.
pub fn strip_vendor(rules: &RuleSet, r: &Rendered) -> Rendered {
    let blocks: Vec<RBlock> = r
        .blocks
        .iter()
        .filter(|b| !matches!(b, RBlock::Opaque { .. } | RBlock::Thinking { .. }))
        .cloned()
        .collect();
    let tokens = estimate_blocks(rules, &blocks);
    Rendered { role: r.role, blocks, tokens, supersedable: r.supersedable }
}

/// Level 2 trimming: every long text inside the rendering keeps only head + tail.
/// Returns `None` when nothing would change.
pub fn trim(rules: &RuleSet, r: &Rendered, keep: usize) -> Option<Rendered> {
    fn go(b: &RBlock, keep: usize, changed: &mut bool) -> RBlock {
        match b {
            RBlock::Text { text } if text.len() > keep * 2 + 64 => {
                *changed = true;
                RBlock::Text { text: preview(text, keep) }
            }
            RBlock::Data { source, text } if text.len() > keep * 2 + 64 => {
                *changed = true;
                RBlock::Data { source: source.clone(), text: preview(text, keep) }
            }
            RBlock::ToolResult { id, content, is_error } => RBlock::ToolResult {
                id: id.clone(),
                content: content.iter().map(|c| go(c, keep, changed)).collect(),
                is_error: *is_error,
            },
            other => other.clone(),
        }
    }
    let mut changed = false;
    let blocks: Vec<RBlock> = r.blocks.iter().map(|b| go(b, keep, &mut changed)).collect();
    if !changed {
        return None;
    }
    let tokens = estimate_blocks(rules, &blocks);
    Some(Rendered { role: r.role, blocks, tokens, supersedable: r.supersedable })
}

/// Level 3: images are replaced by a textual reference to their blob.
pub fn offload_images(rules: &RuleSet, r: &Rendered) -> Option<Rendered> {
    fn go(b: &RBlock, changed: &mut bool) -> RBlock {
        match b {
            RBlock::Image { blob } => {
                *changed = true;
                RBlock::Text {
                    text: format!("[image offloaded: blob sha256:{} ({} bytes)]", blob.sha256, blob.size),
                }
            }
            RBlock::ToolResult { id, content, is_error } => RBlock::ToolResult {
                id: id.clone(),
                content: content.iter().map(|c| go(c, changed)).collect(),
                is_error: *is_error,
            },
            other => other.clone(),
        }
    }
    let mut changed = false;
    let blocks: Vec<RBlock> = r.blocks.iter().map(|b| go(b, &mut changed)).collect();
    if !changed {
        return None;
    }
    let tokens = estimate_blocks(rules, &blocks);
    Some(Rendered { role: r.role, blocks, tokens, supersedable: r.supersedable })
}

/// Fixed text that replaces an erased (tombstoned) body.
pub const ERASED: &str = "[erased]";

/// The fixed rendering of a tombstoned event. Structure needed for tool
/// pairing survives (tool_use ids and names, tool_result ids); every body is
/// replaced by [`ERASED`], tool inputs by `{}`, and vendor-private blocks
/// (thinking, opaque) are dropped.
pub fn erased(rules: &RuleSet, r: &Rendered) -> Rendered {
    let mut blocks: Vec<RBlock> = Vec::new();
    let text = || RBlock::Text { text: ERASED.to_string() };
    for b in &r.blocks {
        match b {
            RBlock::ToolUse { id, name, .. } => {
                blocks.push(RBlock::ToolUse { id: id.clone(), name: name.clone(), input: serde_json::json!({}) })
            }
            RBlock::ToolResult { id, is_error, .. } => {
                blocks.push(RBlock::ToolResult { id: id.clone(), content: vec![text()], is_error: *is_error })
            }
            RBlock::Thinking { .. } | RBlock::Opaque { .. } => {}
            RBlock::Text { .. } | RBlock::Guidance { .. } | RBlock::Data { .. } | RBlock::Image { .. } => {
                if !matches!(blocks.last(), Some(RBlock::Text { text }) if text == ERASED) {
                    blocks.push(text());
                }
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(text());
    }
    let tokens = estimate_blocks(rules, &blocks);
    Rendered { role: r.role, blocks, tokens, supersedable: r.supersedable }
}

/// Rendering of a level-4 summary: the fixed note plus the summary text.
pub fn summary(rules: &RuleSet, profile: &RenderProfile, trust: &Trust, text: &str) -> Rendered {
    let blocks = vec![RBlock::Guidance { text: SUMMARY_NOTE.to_string() }, frame(profile, trust, text.to_string())];
    let tokens = estimate_blocks(rules, &blocks);
    Rendered { role: Role::User, blocks, tokens, supersedable: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> RenderProfile {
        RenderProfile::default()
    }

    #[test]
    fn user_verbatim_guidance_and_data() {
        let r = RuleSet::default();
        let ev = Event::UserMessage { text: "hi".into(), attachments: vec![] };
        let out = render(&r, &p(), &Trust::User, &ev).unwrap();
        assert_eq!(out.blocks, vec![RBlock::Text { text: "hi".into() }]);
        let ev = Event::Injected { source: "ci".into(), text: "build red".into() };
        let g = render(&r, &p(), &Trust::Guidance, &ev).unwrap();
        assert!(matches!(&g.blocks[0], RBlock::Guidance { text } if text == "[ci] build red"));
        let d = render(&r, &p(), &Trust::Untrusted { source: "web".into() }, &ev).unwrap();
        match &d.blocks[0] {
            RBlock::Data { source, text } => {
                assert_eq!(source, "web");
                assert!(text.starts_with(&p().data_warning));
            }
            b => panic!("{b:?}"),
        }
        assert!(render(&r, &p(), &Trust::Internal, &Event::Paused).is_none());
    }

    #[test]
    fn tool_result_blob_and_clip() {
        let r = RuleSet::default();
        let mut prof = p();
        prof.inline_limit_bytes = 100;
        prof.preview_bytes = 10;
        let call = ToolCall {
            id: "c".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            access: vec![],
            class: EffectClass::Pure,
        };
        let big = "x".repeat(500);
        let result = ToolResult::text("c".into(), big, false);
        let out = render(&r, &prof, &Trust::Internal, &Event::ToolResulted { call: call.clone(), result }).unwrap();
        let RBlock::ToolResult { content, .. } = &out.blocks[0] else { panic!() };
        let RBlock::Text { text } = &content[0] else { panic!() };
        assert!(text.contains("bytes omitted"));
        assert!(text.len() < 100);
        let blob = BlobRef { sha256: "ab".into(), size: 9000, media_type: None };
        let result = ToolResult {
            call_id: "c".into(),
            content: vec![ToolContent::Blob { blob, preview: "head..tail".into() }],
            is_error: false,
            trust: Trust::Internal,
            observed: vec![],
        };
        let out = render(&r, &prof, &Trust::Internal, &Event::ToolResulted { call, result }).unwrap();
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.contains("head..tail") && s.contains("sha256:ab"));
    }

    #[test]
    fn preview_char_boundaries() {
        let s = "é".repeat(100);
        let p = preview(&s, 7);
        assert!(p.contains("omitted"));
    }
}
