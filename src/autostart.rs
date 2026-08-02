//! Starting the hub on demand, so running an agent is a single command.
//!
//! Agents only see each other when they share one process (Telegram never
//! delivers a bot's message to another bot, so the shared in-memory log is
//! the only path between them). That process used to be the operator's job:
//! open a terminal, run `--hub`, keep it alive, and only then start the MCP
//! clients. Everything below exists to delete that step — the first agent to
//! start brings the hub up, and the rest find it already listening.
//!
//! The hub is deliberately a *detached child*, not a task inside this
//! process. If it lived inside the first agent, closing that one editor would
//! silently take every other agent's connection down with it.

use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::{self, Cli};

/// How long to wait for a freshly spawned hub to accept connections. Generous
/// because the first thing it does is a TLS handshake and a `getMe` per bot.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// A local hub answers `/health` immediately; anything slower is not one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Makes sure a hub is listening, starting one if not.
///
/// Returns the URL to bridge to, once the port accepts connections, so the
/// caller neither races the child's startup nor has to guess the address —
/// the config file may move the hub off the default port.
pub async fn ensure_hub(cli: &Cli, agent: Option<&str>) -> Result<String> {
    let config = config::load(cli.config_path.clone())?;

    // A misspelled agent would otherwise surface much later, as an MCP error
    // from a hub the user did not know had started.
    if let Some(name) = agent
        && config
            .agents
            .iter()
            .all(|a| a.id.to_string() != name.to_lowercase())
    {
        bail!(
            "no agent named {name:?} is configured. Available: [{}]. Add it to {}.",
            config.agent_names(),
            config::describe_source(&cli.config_path)
        );
    }

    let addr = config::parse_addr(
        config
            .http_addr
            .as_deref()
            .unwrap_or(config::DEFAULT_HTTP_ADDR),
    )?;

    let url = format!("http://{addr}/mcp");

    match probe(&url).await {
        Probe::Hub { version } => {
            if version != env!("CARGO_PKG_VERSION") {
                // Upgrading the package does not replace a hub that is
                // already running, so this is what an upgrade looks like from
                // the inside: new tools on the client, old behaviour serving
                // them, and nothing saying why.
                tracing::warn!(
                    "the hub at {addr} is running {version} but this is {}. It keeps serving the                      older code until it is restarted — close every agent, then start one again.",
                    env!("CARGO_PKG_VERSION")
                );
            }
            tracing::info!("using the hub already running at {addr}");
            return Ok(url);
        }
        // Starting a second hub would only fail to bind, and every later
        // error would point at MCP rather than at the port conflict.
        Probe::Stranger => bail!(
            "something that is not a telegram-agent-mcp hub is listening on {addr}. Free that              port, or set a different one under [server] http_addr in {}.",
            config::describe_source(&cli.config_path)
        ),
        Probe::Closed => {}
    }

    let log = log_path();
    tracing::info!(
        "no hub running; starting one at {addr} (log: {})",
        log.display()
    );
    spawn_detached(cli, &addr, &log)?;

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if matches!(probe(&url).await, Probe::Hub { .. }) {
            tracing::info!("hub is up at {addr}");
            return Ok(url);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    bail!(
        "started a hub but nothing is listening on {addr} after {STARTUP_TIMEOUT:?}. \
         Its output is in {} — the usual cause is a bad bot token or the port being held \
         by an unrelated process.",
        log.display()
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// A hub, ready to serve, running the given version.
    Hub { version: String },
    /// Nothing listening, so the port is ours to take.
    Closed,
    /// Someone else's server. Distinguishing this from `Hub` is the whole
    /// reason the hub serves `/health`.
    Stranger,
}

pub async fn probe(url: &str) -> Probe {
    let health = url.trim_end_matches("/mcp").to_string() + "/health";
    let client = reqwest::Client::new();
    let resp = tokio::time::timeout(PROBE_TIMEOUT, client.get(&health).send()).await;
    match resp {
        Ok(Ok(r)) => match r.text().await {
            Ok(body) => match hub_version(&body) {
                Some(version) => Probe::Hub { version },
                None => Probe::Stranger,
            },
            Err(_) => Probe::Stranger,
        },
        // A connection error is the port being closed; anything slower than
        // the timeout is treated the same way, since a hub answers instantly.
        Ok(Err(_)) | Err(_) => Probe::Closed,
    }
}

/// Where the detached hub's logs go. Without this its output would be
/// discarded, and a hub that fails to start would leave nothing to read.
pub fn log_path() -> PathBuf {
    match config::config_dir() {
        Some(dir) => {
            let _ = std::fs::create_dir_all(&dir);
            dir.join("hub.log")
        }
        None => std::env::temp_dir().join("telegram-agent-mcp-hub.log"),
    }
}

fn spawn_detached(cli: &Cli, addr: &SocketAddr, log: &PathBuf) -> Result<()> {
    let exe = std::env::current_exe().context("locating this executable to start the hub")?;

    let mut cmd = Command::new(exe);
    cmd.arg("--hub").arg(addr.to_string());

    // Pass the resolved path rather than relying on the child's working
    // directory, which is not ours once it is detached.
    if let Some(path) = config::resolve_config_path(cli.config_path.clone()) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        cmd.arg("--config").arg(path);
    }

    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening the hub log {}", log.display()))?;
    let err = out.try_clone().context("duplicating the hub log handle")?;

    cmd.stdin(Stdio::null()).stdout(out).stderr(err);
    detach(&mut cmd);

    cmd.spawn()
        .with_context(|| format!("starting the hub ({})", log.display()))?;
    Ok(())
}

/// Cuts the hub loose from this process, so it outlives the agent that
/// happened to start it and does not receive its Ctrl-C.
#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_cmd: &mut Command) {}

/// Reads the version out of a `/health` body, or `None` if this is not one of
/// ours. The body is `telegram-agent-mcp <version> agents=[...]`.
fn hub_version(body: &str) -> Option<String> {
    let rest = body.strip_prefix(crate::http::HEALTH_MARKER)?;
    Some(
        rest.split_whitespace()
            .next()
            .unwrap_or("unknown")
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_health_body_yields_a_version() {
        assert_eq!(
            hub_version(
                "telegram-agent-mcp 0.4.0 agents=[a, b]
"
            )
            .as_deref(),
            Some("0.4.0")
        );
    }

    #[test]
    fn anything_else_on_the_port_is_not_a_hub() {
        // Whatever else answers, it must not be mistaken for one — that is
        // the entire reason /health exists rather than a bare TCP connect.
        assert_eq!(hub_version("<!doctype html><title>404</title>"), None);
        assert_eq!(hub_version(""), None);
    }
}
