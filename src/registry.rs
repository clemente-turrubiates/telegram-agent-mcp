//! The set of agents running in this process, and how a client selects one.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use crate::config::ServerConfig;
use crate::telegram::{AgentId, AgentSession, Hub};

struct State {
    by_name: HashMap<AgentId, Arc<AgentSession>>,
    /// Configuration order, so listings and the default are deterministic.
    order: Vec<AgentId>,
    /// Agent used when a client does not identify itself. `None` when there
    /// is more than one agent and no explicit primary, because guessing would
    /// send messages from the wrong bot.
    default: Option<AgentId>,
}

/// The agents running in this hub, and everything needed to add more at
/// runtime via [`AgentRegistry::reload`] — the hub they share and the HTTP
/// client their sessions poll with.
///
/// Reload only ever *adds*: an agent already present is left exactly as it
/// is, even if its token changed in the file, since swapping a running
/// agent's identity out from under an active poller is a distinct, riskier
/// operation this does not attempt. A newly added agent is reachable
/// immediately at `/mcp?agent=<name>`, which resolves against this registry
/// per request — but not at the dedicated `/mcp/<agent>` route, which
/// [`crate::http::router`] builds once from the agents present at startup.
pub struct AgentRegistry {
    hub: Arc<Hub>,
    http: reqwest::Client,
    state: RwLock<State>,
}

fn read(state: &RwLock<State>) -> RwLockReadGuard<'_, State> {
    state
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write(state: &RwLock<State>) -> RwLockWriteGuard<'_, State> {
    state
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl AgentRegistry {
    /// Builds every configured agent on one shared hub. The hub is what makes
    /// the agents visible to each other.
    pub fn build(config: &ServerConfig, hub: Arc<Hub>, http: reqwest::Client) -> Result<Arc<Self>> {
        let registry = Arc::new(Self {
            hub,
            http,
            state: RwLock::new(State {
                by_name: HashMap::new(),
                order: Vec::new(),
                default: None,
            }),
        });
        registry.reload(config);
        Ok(registry)
    }

    /// Adds every agent in `config` not already present, and recomputes the
    /// default agent from the full current set. Returns the sessions that
    /// were actually added, so the caller can start a poller for each —
    /// reload never starts one itself, to keep "who has a poller running"
    /// answerable from one place.
    pub fn reload(&self, config: &ServerConfig) -> Vec<Arc<AgentSession>> {
        let mut added = Vec::new();
        let mut state = write(&self.state);
        for agent in &config.agents {
            if state.by_name.contains_key(&agent.id) {
                continue;
            }
            let session = Arc::new(AgentSession::new(
                agent.id.clone(),
                Arc::clone(&self.hub),
                self.http.clone(),
                agent.token.clone(),
                agent.profile.clone(),
            ));
            state.order.push(agent.id.clone());
            state.by_name.insert(agent.id.clone(), Arc::clone(&session));
            added.push(session);
        }
        state.default = config.default_agent().map(|a| a.id.clone());
        added
    }

    pub fn sessions(&self) -> Vec<Arc<AgentSession>> {
        let state = read(&self.state);
        state
            .order
            .iter()
            .filter_map(|id| state.by_name.get(id).cloned())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        read(&self.state)
            .order
            .iter()
            .map(|id| id.to_string())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentSession>> {
        let id = AgentId::parse(name).ok()?;
        read(&self.state).by_name.get(&id).cloned()
    }

    pub fn default_session(&self) -> Option<Arc<AgentSession>> {
        let state = read(&self.state);
        state
            .default
            .as_ref()
            .and_then(|id| state.by_name.get(id))
            .cloned()
    }

    /// Message shown when a client did not select a valid agent. Lists the
    /// real names, since the usual cause is a typo or a missing `?agent=`.
    pub fn selection_help(&self) -> String {
        format!(
            "no agent selected. Connect to /mcp/<agent> or /mcp?agent=<name>, where <name> is \
             one of [{}].",
            self.names().join(", ")
        )
    }

    /// Starts one poller per agent. Each keeps its own getUpdates offset, but
    /// they all ingest into the shared hub, which deduplicates the copies
    /// every bot receives of the same human message.
    pub fn spawn_pollers(&self) {
        for session in self.sessions() {
            spawn_poller(session);
        }
    }
}

/// How long to wait before restarting a poller that stopped.
const POLLER_RESTART_DELAY: Duration = Duration::from_secs(5);

/// Runs one agent's poller under supervision, restarting it if it ever stops.
///
/// `run_poller_forever` already handles request errors, so the only ways out
/// are a panic or a bug. Both matter more than they look: a bare
/// `tokio::spawn` drops the `JoinHandle`, so a panicking poller ends that
/// agent's ingest permanently and *silently* — no log line, and the only
/// symptom is a bot that stops hearing anything, noticed hours later.
pub fn spawn_poller(session: Arc<AgentSession>) {
    tokio::spawn(async move {
        loop {
            let attempt = tokio::spawn(Arc::clone(&session).run_poller_forever());
            match attempt.await {
                Ok(()) => tracing::error!(
                    agent = %session.id(),
                    "poller returned unexpectedly; restarting in {POLLER_RESTART_DELAY:?}"
                ),
                Err(err) if err.is_cancelled() => return,
                Err(err) => tracing::error!(
                    agent = %session.id(),
                    "poller panicked ({err}); restarting in {POLLER_RESTART_DELAY:?}. This agent \
                     received nothing while it was down."
                ),
            }
            tokio::time::sleep(POLLER_RESTART_DELAY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::AgentSelfProfile;

    /// `reqwest::Client::new()` panics without one; the test binary never
    /// hits `main`'s own one-time install, so each test that builds a client
    /// needs this itself. `Once` makes it safe to call from several tests
    /// running in parallel.
    fn ensure_crypto_provider() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    fn config(agents: Vec<(&str, &str)>) -> ServerConfig {
        ServerConfig {
            agents: agents
                .into_iter()
                .map(|(n, t)| crate::config::AgentConfig {
                    id: AgentId::parse(n).unwrap(),
                    token: t.into(),
                    profile: AgentSelfProfile::default(),
                })
                .collect(),
            primary: None,
            http_addr: None,
            idle_shutdown_secs: None,
        }
    }

    #[test]
    fn reload_adds_new_agents_without_disturbing_existing_ones() {
        ensure_crypto_provider();
        let registry = AgentRegistry::build(
            &config(vec![("alpha", "111:aaa")]),
            Arc::new(Hub::new()),
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(registry.names(), vec!["alpha"]);
        let alpha_before = registry.get("alpha").unwrap();

        let added = registry.reload(&config(vec![("alpha", "111:aaa"), ("beta", "222:bbb")]));
        assert_eq!(added.len(), 1, "only the new agent should be reported");
        assert_eq!(added[0].id().to_string(), "beta");

        assert_eq!(registry.names(), vec!["alpha", "beta"]);
        assert!(
            Arc::ptr_eq(&alpha_before, &registry.get("alpha").unwrap()),
            "an existing agent's session must not be replaced on reload"
        );
    }

    #[test]
    fn reload_is_idempotent() {
        ensure_crypto_provider();
        let registry = AgentRegistry::build(
            &config(vec![("alpha", "111:aaa")]),
            Arc::new(Hub::new()),
            reqwest::Client::new(),
        )
        .unwrap();
        let added = registry.reload(&config(vec![("alpha", "111:aaa")]));
        assert!(added.is_empty(), "nothing new means nothing added");
        assert_eq!(registry.names(), vec!["alpha"]);
    }

    #[test]
    fn reload_recomputes_the_default_from_the_full_set() {
        ensure_crypto_provider();
        let registry = AgentRegistry::build(
            &config(vec![("alpha", "111:aaa")]),
            Arc::new(Hub::new()),
            reqwest::Client::new(),
        )
        .unwrap();
        // A lone agent is its own default.
        assert!(registry.default_session().is_some());

        registry.reload(&config(vec![("alpha", "111:aaa"), ("beta", "222:bbb")]));
        // Two agents with no explicit primary is ambiguous, same as at
        // startup — hot-adding a second identity must not leave a stale
        // default pointed at whichever one happened to be first.
        assert!(registry.default_session().is_none());
    }
}
