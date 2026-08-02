//! The set of agents running in this process, and how a client selects one.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::config::ServerConfig;
use crate::telegram::{AgentId, AgentSession, Hub};

pub struct AgentRegistry {
    by_name: HashMap<AgentId, Arc<AgentSession>>,
    /// Configuration order, so listings and the default are deterministic.
    order: Vec<AgentId>,
    /// Agent used when a client does not identify itself. `None` when there
    /// is more than one agent and no explicit primary, because guessing would
    /// send messages from the wrong bot.
    default: Option<AgentId>,
}

impl AgentRegistry {
    /// Builds every configured agent on one shared hub. The hub is what makes
    /// the agents visible to each other.
    pub fn build(config: &ServerConfig, hub: Arc<Hub>, http: reqwest::Client) -> Result<Arc<Self>> {
        let mut by_name = HashMap::new();
        let mut order = Vec::new();

        for agent in &config.agents {
            let session = Arc::new(AgentSession::new(
                agent.id.clone(),
                Arc::clone(&hub),
                http.clone(),
                agent.token.clone(),
                agent.profile.clone(),
            ));
            order.push(agent.id.clone());
            by_name.insert(agent.id.clone(), session);
        }

        Ok(Arc::new(Self {
            default: config.default_agent().map(|a| a.id.clone()),
            by_name,
            order,
        }))
    }

    pub fn sessions(&self) -> Vec<Arc<AgentSession>> {
        self.order
            .iter()
            .filter_map(|id| self.by_name.get(id).cloned())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.order.iter().map(|id| id.to_string()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentSession>> {
        let id = AgentId::parse(name).ok()?;
        self.by_name.get(&id).cloned()
    }

    pub fn default_session(&self) -> Option<Arc<AgentSession>> {
        self.default
            .as_ref()
            .and_then(|id| self.by_name.get(id))
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
