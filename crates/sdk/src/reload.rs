//! Hot reload: configuration changes are recompiled and sent to every live
//! session as `Control::Reconfigure`; the kernel applies them at the session's
//! next idle point (a busy session journals them as pending), so tool
//! definitions never change mid-turn. Hooks and auto-answer rules are swapped
//! in the gate executor at once. The tool set (built-ins, MCP servers,
//! sub-agent definitions) is fixed when the agent is built; restart to change it.

use crate::agent::{compile_profile, discover_options, gate_chain, kernel_config, Assembly, Built, Compiled, Config};
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
}

/// What a reload needs besides the configuration.
pub(crate) struct Reloader {
    pub assembly: Assembly,
    pub gates: Arc<ReloadableGates>,
    pub hook_env: HookEnv,
}

/// Recompile and reconfigure the live sessions (no-op for them when the
/// kernel configuration did not change).
pub(crate) async fn reload(cfg: &Config, b: &Built) -> Result<Profile, Error> {
    let r = &b.reloader;
    let profile = compile_profile(cfg, &r.assembly.workspace)?.with_tools(r.assembly.specs.clone());
    let unattended = cfg.unattended.or(profile.kernel.unattended);
    let chain = gate_chain(cfg, &profile, &r.hook_env, &r.assembly.workspace, unattended, r.assembly.disposable);
    let assembly = Assembly { unattended, hooked: chain.hooked_points(), ..r.assembly.clone() };
    let config = kernel_config(cfg, &profile, &assembly);
    r.gates.set(chain);
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
