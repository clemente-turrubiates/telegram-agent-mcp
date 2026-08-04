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

/// How long an autostarted hub waits with no bridge client connected before
/// exiting on its own. Nobody launched this hub directly, so nobody would
/// otherwise think to stop it — long enough to survive an editor restart or
/// a client briefly reconnecting, short enough not to sit idle for hours.
const AUTOSTART_IDLE_SHUTDOWN: Duration = Duration::from_secs(600);

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
        Probe::Hub { version, .. } => {
            if version != env!("CARGO_PKG_VERSION") {
                // Upgrading the package does not replace a hub that is
                // already running, so this is what an upgrade looks like from
                // the inside: new tools on the client, old behaviour serving
                // them, and nothing saying why.
                tracing::warn!(
                    "the hub at {addr} is running {version} but this is {}. It keeps serving the \
                     older code until it is restarted — close every agent, then start one again.",
                    env!("CARGO_PKG_VERSION")
                );
            }
            tracing::info!("using the hub already running at {addr}");
            return Ok(url);
        }
        // Starting a second hub would only fail to bind, and every later
        // error would point at MCP rather than at the port conflict.
        Probe::Stranger => bail!(
            "something that is not a telegram-agent-mcp hub is listening on {addr}. Free that \
             port, or set a different one under [server] http_addr in {}.",
            config::describe_source(&cli.config_path)
        ),
        Probe::Closed => {}
    }

    let log = log_path();
    tracing::info!(
        "no hub running; starting one at {addr} (log: {})",
        log.display()
    );
    spawn_detached(cli, &addr, &log, config.idle_shutdown_secs)?;

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
    Hub {
        version: String,
        /// Bridge clients currently attached, if the hub's `/health` reported
        /// one (older versions of this line didn't).
        clients: Option<usize>,
        /// Idle-shutdown timeout in seconds, if the hub has one armed.
        idle_shutdown_secs: Option<u64>,
        /// Seconds the hub has been continuously idle, if it is right now.
        idle_for_secs: Option<u64>,
    },
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
            Ok(body) => match parse_health_body(&body) {
                Some(info) => Probe::Hub {
                    version: info.version,
                    clients: info.clients,
                    idle_shutdown_secs: info.idle_shutdown_secs,
                    idle_for_secs: info.idle_for_secs,
                },
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

fn spawn_detached(
    cli: &Cli,
    addr: &SocketAddr,
    log: &PathBuf,
    idle_shutdown_secs: Option<u64>,
) -> Result<()> {
    let exe = std::env::current_exe().context("locating this executable to start the hub")?;

    // An autostarted hub always gets idle shutdown — nobody launched it
    // directly, so nobody would otherwise think to stop it. `[server]
    // idle_shutdown_secs` in the config file overrides the default, for
    // tuning it once instead of passing a flag that isn't there to pass.
    let idle_shutdown_secs = idle_shutdown_secs.unwrap_or(AUTOSTART_IDLE_SHUTDOWN.as_secs());

    let mut cmd = Command::new(exe);
    cmd.arg("--hub")
        .arg(addr.to_string())
        .arg("--idle-shutdown")
        .arg(idle_shutdown_secs.to_string());

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

/// What `/health` says about itself, or `None` if this is not one of ours.
struct HealthInfo {
    version: String,
    clients: Option<usize>,
    idle_shutdown_secs: Option<u64>,
    idle_for_secs: Option<u64>,
}

/// Parses a `/health` body: `telegram-agent-mcp <version> agents=[...]
/// clients=<n> idle_after=<secs|-> idle_for=<secs|->`. Only the marker and
/// the version are guaranteed present — the rest are read leniently, token by
/// token, so an older hub's shorter line (before those fields existed) still
/// parses as a hub, just with less to report.
fn parse_health_body(body: &str) -> Option<HealthInfo> {
    let rest = body.strip_prefix(crate::http::HEALTH_MARKER)?;
    let mut tokens = rest.split_whitespace();
    let version = tokens.next().unwrap_or("unknown").to_string();

    let mut info = HealthInfo {
        version,
        clients: None,
        idle_shutdown_secs: None,
        idle_for_secs: None,
    };
    for token in tokens {
        if let Some(v) = token.strip_prefix("clients=") {
            info.clients = v.parse().ok();
        } else if let Some(v) = token.strip_prefix("idle_after=") {
            info.idle_shutdown_secs = v.parse().ok();
        } else if let Some(v) = token.strip_prefix("idle_for=") {
            info.idle_for_secs = v.parse().ok();
        }
    }
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_health_body_yields_a_version() {
        assert_eq!(
            parse_health_body(
                "telegram-agent-mcp 0.4.0 agents=[a, b]
"
            )
            .unwrap()
            .version,
            "0.4.0"
        );
    }

    #[test]
    fn an_older_hub_without_the_newer_fields_still_parses() {
        // A hub built before clients=/idle_after=/idle_for= existed must
        // still be recognised as a hub, just with less to report.
        let info = parse_health_body("telegram-agent-mcp 0.4.0 agents=[a, b]\n").unwrap();
        assert_eq!(info.clients, None);
        assert_eq!(info.idle_shutdown_secs, None);
        assert_eq!(info.idle_for_secs, None);
    }

    #[test]
    fn client_and_idle_fields_are_parsed_when_present() {
        let info = parse_health_body(
            "telegram-agent-mcp 0.4.1 agents=[a] clients=2 idle_after=600 idle_for=-\n",
        )
        .unwrap();
        assert_eq!(info.clients, Some(2));
        assert_eq!(info.idle_shutdown_secs, Some(600));
        assert_eq!(info.idle_for_secs, None, "\"-\" means not currently idle");

        let idle = parse_health_body(
            "telegram-agent-mcp 0.4.1 agents=[a] clients=0 idle_after=600 idle_for=45\n",
        )
        .unwrap();
        assert_eq!(idle.idle_for_secs, Some(45));
    }

    #[test]
    fn anything_else_on_the_port_is_not_a_hub() {
        // Whatever else answers, it must not be mistaken for one — that is
        // the entire reason /health exists rather than a bare TCP connect.
        assert!(parse_health_body("<!doctype html><title>404</title>").is_none());
        assert!(parse_health_body("").is_none());
    }
}
