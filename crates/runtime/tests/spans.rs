//! Tracing spans (own test binary: the capturing subscriber is installed as
//! the global default, which must not race with other tests).
mod common;

use agent_kernel::Decision;
use agent_proto::*;
use agent_runtime::*;
use common::*;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

// ---------------------------------------------------------------- spans

mod capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::Subscriber;
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::Layer;

    #[derive(Debug, Clone, Default)]
    pub struct SpanRec {
        pub name: String,
        pub parent: Option<String>,
        pub fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    pub struct Capture(pub Arc<Mutex<BTreeMap<u64, SpanRec>>>);

    struct V<'a>(&'a mut BTreeMap<String, String>);
    impl Visit for V<'_> {
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            self.0.insert(f.name().to_string(), format!("{v:?}"));
        }
        fn record_str(&mut self, f: &Field, v: &str) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            let mut rec = SpanRec { name: attrs.metadata().name().to_string(), ..Default::default() };
            attrs.record(&mut V(&mut rec.fields));
            rec.parent = ctx.span(id).and_then(|s| s.parent()).map(|p| p.name().to_string());
            self.0.lock().unwrap().insert(id.into_u64(), rec);
        }
        fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
            if let Some(rec) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
                values.record(&mut V(&mut rec.fields));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spans_cover_session_turn_and_effect_with_parent_link() {
    use tracing_subscriber::prelude::*;
    let cap = capture::Capture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(cap.clone())).unwrap();

    let rt: Runtime<Toy> = Runtime::builder().model(ScriptModel::new(vec![text_reply("hi")])).options(options()).build();
    // A child session whose SessionStarted names its parent, starting a turn
    // with a sample.
    let effect = EffectId { epoch: 0, n: 0 };
    let initial = Decision {
        events: vec![
            Draft::internal(Event::SessionStarted {
                session: sid("child"),
                profile_hash: "h".into(),
                config: KernelConfig::default(),
                parent_session: Some(sid("parent-1")),
            }),
            Draft::internal(Event::TurnStarted { cause: TurnCause::User }),
            Draft::internal(Event::EffectIssued { id: effect, effect: Effect::Sample(prompt()) }),
        ],
        effects: vec![(effect, Effect::Sample(prompt()))],
    };
    let h = rt.create_session(sid("child"), initial).await.unwrap();
    eventually(|| h.finish_count() == 1).await;

    let spans: Vec<capture::SpanRec> = cap.0.lock().unwrap().values().cloned().collect();
    let session = spans.iter().find(|s| s.name == "session").expect("session span");
    assert_eq!(session.fields.get("session.id").map(String::as_str), Some("child"));
    assert_eq!(session.fields.get("session.parent").map(String::as_str), Some("parent-1"));
    let turn = spans.iter().find(|s| s.name == "turn").expect("turn span");
    assert_eq!(turn.parent.as_deref(), Some("session"));
    let eff = spans
        .iter()
        .find(|s| s.name == "effect" && s.fields.get("effect.kind").map(String::as_str) == Some("sample"))
        .unwrap_or_else(|| panic!("effect span: {spans:#?}"));
    assert_eq!(eff.parent.as_deref(), Some("turn"));
    assert_eq!(eff.fields.get("effect.id").map(String::as_str), Some(effect.to_string().as_str()));
    assert_eq!(eff.fields.get("session.id").map(String::as_str), Some("child"));
}

