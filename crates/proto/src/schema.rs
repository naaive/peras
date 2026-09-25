//! JSON Schema export. Clients depend only on these schemas.

use schemars::schema::RootSchema;
use std::collections::BTreeMap;

/// All exported top-level schemas, keyed by type name.
pub fn export_all() -> BTreeMap<&'static str, RootSchema> {
    let mut m = BTreeMap::new();
    m.insert("Envelope", schemars::schema_for!(crate::Envelope<crate::Event>));
    m.insert("Event", schemars::schema_for!(crate::Event));
    m.insert("Input", schemars::schema_for!(crate::Input));
    m.insert("Effect", schemars::schema_for!(crate::Effect));
    m.insert("EffectResult", schemars::schema_for!(crate::EffectResult));
    m.insert("Signal", schemars::schema_for!(crate::Signal));
    m.insert("Control", schemars::schema_for!(crate::Control));
    m.insert("Verdict", schemars::schema_for!(crate::Verdict));
    m.insert("ClientMessage", schemars::schema_for!(crate::ClientMessage));
    m.insert("ServerMessage", schemars::schema_for!(crate::ServerMessage));
    m.insert("KernelConfig", schemars::schema_for!(crate::KernelConfig));
    m
}

/// Pretty JSON of all schemas.
pub fn export_json() -> String {
    serde_json::to_string_pretty(&export_all()).expect("schemas serialize")
}

#[cfg(test)]
mod tests {
    #[test]
    fn exports() {
        let s = super::export_json();
        assert!(s.contains("session_started"));
        assert!(s.contains("ClientMessage"));
    }
}
