//! Hot reload: configuration changes are recompiled and sent to every live
//! session as `Control::Reconfigure`; the kernel applies them at the session's
//! next idle point (a busy session journals them as pending), so tool
//! definitions never change mid-turn. Hooks and auto-answer rules are swapped
//! in the gate executor at once.
//!
//! The tool set is reassembled too ([`Toolbox`]): MCP servers added, changed
//! or removed (unchanged ones keep their connection), sub-agent definitions
//! added, changed or removed (unchanged ones keep their runtime and live
//! child sessions), default built-ins (the skills catalog) and shell rules.
//! The runtime's registry is replaced at once, but what the model sees and
//! may call only changes when each session applies the new configuration at
//! its next idle point (removed tools are then denied, added ones load with
//! the next request sequence). While a session is busy, removed tools stay
//! registered so its turn can still finish with the definitions it started
//! with; they are dropped by a later reload that finds every session idle.
//!
//! Not reloaded, because they are fixed when the agent is built:
//! - the model port, journal, sandbox and checkpointer (live sessions and
//!   in-flight effects hold them; changing them is a restart);
//! - observers (each is attached to the runtime with its own persistent
//!   delivery cursor when it is built);
//! - whether the agent takes the workspace lock (decided by the tool set at
//!   build; a read-only agent that gains writing tools by a reload does not
//!   start taking it mid-process).

use crate::agent::{compile_profile, discover_options, gate_chain, kernel_config, Assembly, Built, Compiled, Config, Toolbox};
use crate::error::Error;
use crate::hooks::HookEnv;
use agent_profile::Profile;
use agent_proto::*;
use agent_runtime::*;
use async_trait::async_trait;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Changes are applied once the files have been quiet for this long.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// A gate executor whose chain can be replaced (hooks reloaded).
pub(crate) struct ReloadableGates {
    chain: RwLock<Arc<GateChain>>,
}

impl ReloadableGates {
    pub(crate) fn new(chain: GateChain) -> Self {
        ReloadableGates { chain: RwLock::new(Arc::new(chain)) }
    }

    fn current(&self) -> Arc<GateChain> {
        self.chain.read().unwrap().clone()
    }

    fn set(&self, chain: GateChain) {
        *self.chain.write().unwrap() = Arc::new(chain);
    }
}

#[async_trait]
impl GateExecutor for ReloadableGates {
    async fn evaluate(&self, req: &GateRequest) -> (Verdict, Responder) {
        self.current().evaluate(req).await
    }
    async fn evaluate_in(&self, req: &GateRequest, ctx: &GateCtx) -> (Verdict, Responder) {
        self.current().evaluate_in(req, ctx).await
    }
    async fn evaluate_outcome(&self, req: &GateRequest, ctx: &GateCtx) -> GateOutcome {
        self.current().evaluate_outcome(req, ctx).await
    }
    fn auto_answer(&self, req: &GateRequest) -> Option<(Answer, Responder)> {
        self.current().auto_answer(req)
    }
}

/// What a reload needs besides the configuration.
pub(crate) struct Reloader {
    assembly: std::sync::Mutex<Assembly>,
    pub gates: Arc<ReloadableGates>,
    hook_env: std::sync::Mutex<HookEnv>,
    toolbox: Toolbox,
    /// One reload at a time (the watcher and `Agent::reload`).
    serial: tokio::sync::Mutex<()>,
}

impl Reloader {
    pub(crate) fn new(assembly: Assembly, gates: Arc<ReloadableGates>, hook_env: HookEnv, toolbox: Toolbox) -> Self {
        Reloader {
            assembly: std::sync::Mutex::new(assembly),
            gates,
            hook_env: std::sync::Mutex::new(hook_env),
            toolbox,
            serial: tokio::sync::Mutex::new(()),
        }
    }
}

/// Recompile, reassemble the tool set and reconfigure the live sessions
/// (no-op for them when the kernel configuration did not change).
pub(crate) async fn reload(cfg: &Config, b: &Built) -> Result<Profile, Error> {
    let r = &b.reloader;
    let _serial = r.serial.lock().await;
    let base = r.assembly.lock().unwrap().clone();
    let profile = compile_profile(cfg, &base.workspace)?;
    let (mut registry, mcp, agents) = r.toolbox.assemble(cfg, &base.workspace, &profile).await;
    let specs = registry.specs();
    // A busy session still runs on the definitions its turn started with.
    let busy = b.rt.live_sessions().iter().any(|h| h.with_state(|s| agent_kernel::phase(s) != agent_kernel::Phase::Idle));
    if busy {
        let old = b.rt.tools();
        for name in old.names() {
            if registry.get(&name).is_none() {
                if let Some(t) = old.get(&name) {
                    registry.register(t.clone());
                }
            }
        }
    }
    b.rt.set_tools(registry);
    let profile = profile.with_tools(specs.clone());
    let hook_env = HookEnv { mcp, agents, ..r.hook_env.lock().unwrap().clone() };
    let unattended = cfg.unattended.or(profile.kernel.unattended);
    let chain = gate_chain(cfg, &profile, &hook_env, &base.workspace, unattended, base.disposable);
    let assembly = Assembly { specs, unattended, hooked: chain.hooked_points(), ..base };
    let config = kernel_config(cfg, &profile, &assembly);
    r.gates.set(chain);
    *r.hook_env.lock().unwrap() = hook_env;
    *r.assembly.lock().unwrap() = assembly;
    let changed = b.compiled().config != config;
    let profile_hash = format!("{}:{}", profile.hash, agent_kernel::config_hash(&config));
    *b.compiled.write().unwrap() = Compiled { config: config.clone(), profile_hash, profile: profile.clone() };
    if changed {
        for h in b.rt.live_sessions() {
            let reconfigure = Input::Control(Control::Reconfigure { config: Box::new(config.clone()) });
            if let Err(e) = h.post(reconfigure) {
                tracing::warn!(session = %h.id(), error = %e, "reconfigure not delivered");
            }
        }
    }
    Ok(profile)
}

/// Start watching the configuration of a discover-mode agent.
pub(crate) fn watch(cfg: &Config, b: &Arc<Built>) {
    let opts = match discover_options(cfg) {
        Ok(Some(o)) => o,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "hot reload disabled");
            return;
        }
    };
    let specs = agent_profile::config_locations(&opts)
        .into_iter()
        .map(|(path, recursive)| WatchSpec { path, recursive })
        .collect();
    let weak = Arc::downgrade(b);
    let cfg = cfg.clone();
    let on_change = move |paths: Vec<std::path::PathBuf>| {
        let (weak, cfg) = (weak.clone(), cfg.clone());
        Box::pin(async move {
            let Some(b) = weak.upgrade() else { return };
            match reload(&cfg, &b).await {
                Ok(p) => tracing::info!(?paths, profile = %p.hash, "configuration reloaded"),
                Err(e) => tracing::warn!(?paths, error = %e, "configuration change not applied"),
            }
        }) as futures::future::BoxFuture<'static, ()>
    };
    match ConfigWatcher::spawn(specs, agent_profile::is_config_path, DEBOUNCE, on_change) {
        Ok(w) => *b._watcher.lock().unwrap() = Some(w),
        Err(e) => tracing::warn!(error = %e, "hot reload unavailable"),
    }
}
