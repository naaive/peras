//! Read-time schema upgrades. Old data is never migrated on disk: every read
//! upgrades step by step from the stored `schema` to [`EVENT_SCHEMA`].

use crate::envelope::Envelope;
use crate::event::{Event, EVENT_SCHEMA};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("event schema {0} is newer than supported {EVENT_SCHEMA}")]
    TooNew(u16),
    /// Unknown, non-ignorable event: refuse to load rather than silently drop.
    #[error("unknown event type `{0}` (not marked ignorable)")]
    Unknown(String),
    #[error("malformed event: {0}")]
    Malformed(String),
}

/// One upgrade step: body JSON at version `from` -> version `from + 1`.
type Step = fn(Value) -> Result<Value, UpgradeError>;

/// Steps indexed by source version (`STEPS[0]` upgrades v0 -> v1).
const STEPS: &[Step] = &[v0_to_v1, v1_to_v2];

/// v0 -> v1: v0 had no `attachments` on `user_message`.
fn v0_to_v1(mut v: Value) -> Result<Value, UpgradeError> {
    if v.get("type").and_then(Value::as_str) == Some("user_message") {
        if let Some(o) = v.as_object_mut() {
            o.entry("attachments").or_insert_with(|| Value::Array(vec![]));
        }
    }
    Ok(v)
}

/// v1 -> v2: `effect_issued` of a `sample` / `compact` carried the whole
/// prompt (sequence head + context body). It now carries a reference: the
/// sequence number and the number of context fragments replayed (for a
/// compaction, plus the text of its trailing instruction fragment). The body
/// was always the context at issue time, so nothing is lost.
fn v1_to_v2(mut v: Value) -> Result<Value, UpgradeError> {
    if v.get("type").and_then(Value::as_str) != Some("effect_issued") {
        return Ok(v);
    }
    let Some(effect) = v.get_mut("effect") else { return Ok(v) };
    let kind = effect.get("effect").and_then(Value::as_str).unwrap_or_default();
    let bad = |what: &str| UpgradeError::Malformed(format!("effect_issued {kind}: {what}"));
    let prompt_ref = |p: &Value| -> Result<(u64, usize, u64), UpgradeError> {
        let seq_no = p.pointer("/head/seq_no").and_then(Value::as_u64).ok_or_else(|| bad("no head.seq_no"))?;
        let body = p.get("body").and_then(Value::as_array).ok_or_else(|| bad("no body"))?.len();
        let max_tokens = p.get("max_tokens").and_then(Value::as_u64).ok_or_else(|| bad("no max_tokens"))?;
        Ok((seq_no, body, max_tokens))
    };
    let new = match kind {
        "sample" => {
            let (seq_no, entries, max_tokens) = prompt_ref(effect)?;
            serde_json::json!({"effect": "sample_ref", "seq_no": seq_no, "entries": entries, "max_tokens": max_tokens})
        }
        "compact" => {
            let prompt = effect.get("prompt").ok_or_else(|| bad("no prompt"))?;
            let (seq_no, body, max_tokens) = prompt_ref(prompt)?;
            // The last fragment is the instruction: `Rendered::text(Role::User, ..)`.
            let last = prompt.pointer(&format!("/body/{}", body.checked_sub(1).ok_or_else(|| bad("empty body"))?));
            let instruction = match last {
                Some(r) if r.get("role").and_then(Value::as_str) == Some("user") => {
                    match r.get("blocks").and_then(Value::as_array).map(Vec::as_slice) {
                        Some([b]) if b.get("type").and_then(Value::as_str) == Some("text") => {
                            b.get("text").and_then(Value::as_str).map(str::to_string)
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
            .ok_or_else(|| bad("last fragment is not the instruction"))?;
            serde_json::json!({
                "effect": "compact_ref",
                "seq_no": seq_no,
                "entries": body - 1,
                "instruction": instruction,
                "max_tokens": max_tokens,
                "range": effect.get("range").cloned().unwrap_or(Value::Null),
                "overflow": effect.get("overflow").cloned().unwrap_or(Value::Bool(false)),
            })
        }
        _ => return Ok(v),
    };
    *effect = new;
    Ok(v)
}

/// Upgrade a raw body and decode it.
pub fn upgrade_body(schema: u16, mut body: Value) -> Result<Option<Event>, UpgradeError> {
    if schema > EVENT_SCHEMA {
        return Err(UpgradeError::TooNew(schema));
    }
    for step in &STEPS[schema as usize..EVENT_SCHEMA as usize] {
        body = step(body)?;
    }
    match serde_json::from_value::<Event>(body.clone()) {
        Ok(e) => Ok(Some(e)),
        Err(err) => {
            // Ignorable plugin events of unknown shape are skipped; anything
            // else refuses the load.
            let ty = body.get("type").and_then(Value::as_str).unwrap_or("?").to_string();
            let ignorable = body.get("ignorable").and_then(Value::as_bool).unwrap_or(false);
            if ignorable {
                Ok(None)
            } else if ty == "?" {
                Err(UpgradeError::Malformed(err.to_string()))
            } else {
                Err(UpgradeError::Unknown(ty))
            }
        }
    }
}

/// Decode a stored envelope (as JSON), upgrading its body. `Ok(None)` means an
/// ignorable event that this reader does not understand.
pub fn read_envelope(raw: Value) -> Result<Option<Envelope<Event>>, UpgradeError> {
    let schema = raw.get("schema").and_then(Value::as_u64).unwrap_or(0) as u16;
    let body = raw.get("body").cloned().ok_or_else(|| UpgradeError::Malformed("no body".into()))?;
    let Some(event) = upgrade_body(schema, body)? else { return Ok(None) };
    let mut env_raw = raw;
    env_raw["body"] = serde_json::to_value(&event).map_err(|e| UpgradeError::Malformed(e.to_string()))?;
    env_raw["schema"] = Value::from(EVENT_SCHEMA);
    serde_json::from_value(env_raw).map(Some).map_err(|e| UpgradeError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn v0_user_message_gets_attachments() {
        let e = upgrade_body(0, json!({"type":"user_message","text":"hi"})).unwrap().unwrap();
        assert!(matches!(e, Event::UserMessage { ref attachments, .. } if attachments.is_empty()));
    }

    #[test]
    fn v1_sample_becomes_a_reference() {
        let head = serde_json::to_value(crate::model::SeqHead {
            seq_no: 3,
            model: "m".into(),
            system: vec!["sys".into()],
            tools: vec![],
            render: Default::default(),
            encoder_version: 1,
        })
        .unwrap();
        let body = |n: usize| {
            let mut v: Vec<Value> = (0..n)
                .map(|i| serde_json::to_value(crate::Rendered::text(crate::Role::User, format!("m{i}"))).unwrap())
                .collect();
            v.push(serde_json::to_value(crate::Rendered::text(crate::Role::User, "summarise")).unwrap());
            v
        };
        let id = json!({"epoch": 0, "n": 7});
        let e = upgrade_body(
            1,
            json!({"type":"effect_issued","id":id,"effect":{"effect":"sample","head":head,"body":body(4),"max_tokens":99}}),
        )
        .unwrap()
        .unwrap();
        let Event::EffectIssued { effect: crate::Effect::SampleRef(r), .. } = e else { panic!("{e:?}") };
        assert_eq!(r, crate::SampleRef { seq_no: 3, entries: 5, max_tokens: 99 });

        let e = upgrade_body(
            1,
            json!({"type":"effect_issued","id":id,"effect":{"effect":"compact",
                "prompt":{"head":head,"body":body(2),"max_tokens":50},"range":[4,9],"overflow":true}}),
        )
        .unwrap()
        .unwrap();
        let Event::EffectIssued { effect: crate::Effect::CompactRef(r), .. } = e else { panic!("{e:?}") };
        assert_eq!(
            r,
            crate::CompactRef {
                seq_no: 3,
                entries: 2,
                instruction: "summarise".into(),
                max_tokens: 50,
                range: (4, 9),
                overflow: true
            }
        );
        // Other effects are untouched.
        let e = upgrade_body(1, json!({"type":"effect_issued","id":id,"effect":{"effect":"finish","kind":"interrupted"}}))
            .unwrap()
            .unwrap();
        assert!(matches!(e, Event::EffectIssued { effect: crate::Effect::Finish(_), .. }));
    }

    #[test]
    fn journaled_matches_the_upgrade() {
        let head = crate::model::SeqHead {
            seq_no: 1,
            model: "m".into(),
            system: vec![],
            tools: vec![],
            render: Default::default(),
            encoder_version: 1,
        };
        let body = vec![crate::Rendered::text(crate::Role::User, "a"), crate::Rendered::text(crate::Role::User, "go")];
        let full = crate::Effect::Compact(crate::CompactJob {
            prompt: crate::Prompt { head, body, max_tokens: 10 },
            range: (1, 1),
            overflow: false,
        });
        let raw = json!({"type":"effect_issued","id":{"epoch":0,"n":1},"effect":full});
        let Some(Event::EffectIssued { effect, .. }) = upgrade_body(1, raw).unwrap() else { panic!() };
        assert_eq!(Some(effect), full.journaled());
    }

    #[test]
    fn unknown_ignorable_is_skipped() {
        let r = upgrade_body(1, json!({"type":"from_the_future","ignorable":true})).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn unknown_non_ignorable_refuses() {
        assert!(matches!(
            upgrade_body(1, json!({"type":"from_the_future"})),
            Err(UpgradeError::Unknown(_))
        ));
    }

    #[test]
    fn too_new_refuses() {
        assert!(matches!(upgrade_body(99, json!({})), Err(UpgradeError::TooNew(99))));
    }
}
