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
const STEPS: &[Step] = &[v0_to_v1];

/// v0 -> v1: v0 had no `attachments` on `user_message`.
fn v0_to_v1(mut v: Value) -> Result<Value, UpgradeError> {
    if v.get("type").and_then(Value::as_str) == Some("user_message") {
        if let Some(o) = v.as_object_mut() {
            o.entry("attachments").or_insert_with(|| Value::Array(vec![]));
        }
    }
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
