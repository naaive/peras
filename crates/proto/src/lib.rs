//! `agent-proto`: the data contracts of the framework.
//!
//! Everything here is plain data with serde (and JSON Schema) derives. No IO, no
//! async, no closures. Every other crate depends on this one; clients depend only
//! on the JSON Schema it exports (see [`schema::export_all`]).
//!
//! Containers are always ordered (`BTreeMap`/`Vec`) so that the same input always
//! serializes to the same bytes.

pub mod config;
pub mod effect;
pub mod envelope;
pub mod event;
pub mod ids;
pub mod model;
pub mod protocol;
pub mod render;
pub mod resource;
pub mod schema;
pub mod signal;
pub mod tool;
pub mod upgrade;
pub mod verdict;

pub use config::*;
pub use effect::*;
pub use envelope::*;
pub use event::*;
pub use ids::*;
pub use model::*;
pub use protocol::*;
pub use render::*;
pub use resource::*;
pub use signal::*;
pub use tool::*;
pub use verdict::*;

/// Version of the wire protocol (`protocol` module). Negotiated on connect.
pub const PROTOCOL_VERSION: u32 = 0;

#[cfg(test)]
mod roundtrip {
    use super::*;
    use serde::{de::DeserializeOwned, Serialize};

    fn rt<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
        let s = serde_json::to_string(&v).expect("serialize");
        let back: T = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(v, back, "{s}");
    }

    #[test]
    fn tagged_enums_roundtrip() {
        rt(Verdict::deny("no"));
        rt(Verdict::Allow);
        rt(Verdict::Rewrite(Proposal::UserText("x".into())));
        rt(EffectResult::Executed(vec![ToolResult::text(CallId::new("c1"), "ok", false)]));
        rt(EffectResult::SampleFailed(ModelError::Overflow));
        rt(Input::Streamed(
            EffectId { epoch: 0, n: 1 },
            ToolCall {
                id: "c".into(),
                name: "read".into(),
                input: serde_json::json!({"file":"a"}),
                access: vec![Access::read(ResourceUri::fs("/w/a"))],
                class: EffectClass::Pure,
            },
        ));
        rt(Input::Signal(Signal::Submit { text: "hi".into(), attachments: vec![] }));
        rt(Command::Control(Control::HardInterrupt));
        rt(Event::Replaced(Replacement {
            kind: ReplacementKind::Summary,
            range: (1, 2),
            sources: vec![],
            untrusted_sources: vec![],
            content: vec![Rendered::text(Role::User, "s")],
        }));
        rt(Event::SessionStarted {
            session: "s".into(),
            profile_hash: "h".into(),
            config: KernelConfig::default(),
            parent_session: None,
        });
        rt(Answer::AllowWith(Proposal::UserText("y".into())));
        rt(Envelope {
            id: "01".into(),
            parent: None,
            seq: 0,
            at: Timestamp(1),
            origin: Origin::Hook("fmt".into()),
            trust: Trust::Untrusted { source: "web".into() },
            audience: Audience::Both,
            schema: EVENT_SCHEMA,
            body: Event::Paused,
            rendered: None,
        });
    }
}
