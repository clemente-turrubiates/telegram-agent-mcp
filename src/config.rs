//! How the server is told which agents to run, and in which mode.
//!
//! Configuration comes from, in order of precedence:
//!
//! 1. a TOML file (`--config PATH` or `TELEGRAM_AGENTS_FILE`),
//! 2. a TOML file in one of the well-known locations ([`default_config_path`]),
//! 3. numbered environment variables (`TELEGRAM_AGENT_1_TOKEN`, ...),
//! 4. the original single-bot variables (`TELEGRAM_BOT_TOKEN` plus
//!    `TELEGRAM_AGENT_NAME` / `_MODEL` / `_DESCRIPTION`).
//!
//! The file is the recommended form for more than one agent: it is the only
//! one that keeps bot tokens out of the process environment, where they are
//! visible to every child process, and it gives a parse error per field
//! rather than one opaque failure for the whole set.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::telegram::{AgentId, AgentSelfProfile};

/// Where the MCP server listens when running as a hub.
pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8787";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub id: AgentId,
    pub token: String,
    pub profile: AgentSelfProfile,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub agents: Vec<AgentConfig>,
    /// Agent used when a client does not say which one it is. Only meaningful
    /// with exactly one agent configured, or when set explicitly.
    pub primary: Option<AgentId>,
    /// Listen address from the config file, if any. A `--hub ADDR` flag wins.
    pub http_addr: Option<String>,
    /// Idle-shutdown timeout from the config file, if any. A `--idle-shutdown
    /// SECS` flag wins; this is for setting a preference once instead of
    /// having to pass the flag on every invocation that might start the hub.
    pub idle_shutdown_secs: Option<u64>,
}

impl ServerConfig {
    pub fn find(&self, id: &AgentId) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| &a.id == id)
    }

    pub fn agent_names(&self) -> String {
        self.agents
            .iter()
            .map(|a| a.id.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Rejects configurations that would fail confusingly at runtime rather
    /// than at startup.
    fn validate(&self) -> Result<()> {
        if self.agents.is_empty() {
            bail!(
                "no agents configured: set TELEGRAM_BOT_TOKEN, or list agents in a config file \
                 (--config agents.toml)"
            );
        }

        let mut by_name: HashMap<&AgentId, usize> = HashMap::new();
        for agent in &self.agents {
            *by_name.entry(&agent.id).or_insert(0) += 1;
        }
        if let Some((dup, _)) = by_name.iter().find(|(_, n)| **n > 1) {
            bail!("agent name {dup:?} is used more than once; names must be unique");
        }

        // Two pollers on one token make Telegram 409 each other forever, and
        // the resulting error is far from the cause.
        let mut by_token: HashMap<&str, Vec<&AgentId>> = HashMap::new();
        for agent in &self.agents {
            by_token.entry(&agent.token).or_default().push(&agent.id);
        }
        if let Some((_, names)) = by_token.iter().find(|(_, ids)| ids.len() > 1) {
            let names = names
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "agents [{names}] share one bot token. Telegram allows only one poller per \
                 token, so they would continuously disconnect each other — give each agent \
                 its own bot via @BotFather."
            );
        }

        if let Some(primary) = &self.primary
            && self.find(primary).is_none()
        {
            bail!(
                "primary agent {primary:?} is not one of the configured agents ({})",
                self.agent_names()
            );
        }
        Ok(())
    }

    /// The agent to use when a client did not identify itself. Unambiguous
    /// only when there is one agent or an explicit primary.
    pub fn default_agent(&self) -> Option<&AgentConfig> {
        if let Some(primary) = &self.primary {
            return self.find(primary);
        }
        match self.agents.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// TOML file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default)]
    server: FileServer,
    #[serde(default)]
    agents: Vec<FileAgent>,
}

#[derive(Debug, Default, Deserialize)]
struct FileServer {
    http_addr: Option<String>,
    primary: Option<String>,
    idle_shutdown_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct FileAgent {
    name: String,
    token: String,
    model: Option<String>,
    description: Option<String>,
}

fn from_file(path: &PathBuf) -> Result<ServerConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let parsed: FileConfig =
        toml::from_str(&raw).with_context(|| format!("parsing config file {}", path.display()))?;

    let mut agents = Vec::with_capacity(parsed.agents.len());
    for a in parsed.agents {
        let id = AgentId::parse(&a.name)
            .with_context(|| format!("in config file {}", path.display()))?;
        agents.push(AgentConfig {
            profile: AgentSelfProfile {
                name: Some(id.to_string()),
                model: a.model,
                description: a.description,
            },
            id,
            token: a.token,
        });
    }

    let primary = parsed
        .server
        .primary
        .as_deref()
        .map(AgentId::parse)
        .transpose()?;

    Ok(ServerConfig {
        agents,
        primary,
        http_addr: parsed.server.http_addr,
        idle_shutdown_secs: parsed.server.idle_shutdown_secs,
    })
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// `TELEGRAM_AGENT_1_TOKEN`, `TELEGRAM_AGENT_2_TOKEN`, ... read until a gap.
fn from_numbered_env() -> Option<ServerConfig> {
    let mut agents = Vec::new();
    for n in 1.. {
        let Ok(token) = std::env::var(format!("TELEGRAM_AGENT_{n}_TOKEN")) else {
            break;
        };
        let name = std::env::var(format!("TELEGRAM_AGENT_{n}_NAME"))
            .unwrap_or_else(|_| format!("agent{n}"));
        let Ok(id) = AgentId::parse(&name) else {
            tracing::warn!("skipping TELEGRAM_AGENT_{n}: invalid name {name:?}");
            continue;
        };
        agents.push(AgentConfig {
            profile: AgentSelfProfile {
                name: Some(id.to_string()),
                model: std::env::var(format!("TELEGRAM_AGENT_{n}_MODEL")).ok(),
                description: std::env::var(format!("TELEGRAM_AGENT_{n}_DESCRIPTION")).ok(),
            },
            id,
            token,
        });
    }
    if agents.is_empty() {
        return None;
    }
    let primary = std::env::var("TELEGRAM_PRIMARY_AGENT")
        .ok()
        .and_then(|p| AgentId::parse(&p).ok());
    Some(ServerConfig {
        agents,
        primary,
        http_addr: std::env::var("TELEGRAM_MCP_HTTP_ADDR").ok(),
        idle_shutdown_secs: std::env::var("TELEGRAM_IDLE_SHUTDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok()),
    })
}

/// The original single-bot form. Kept working exactly as before so existing
/// installs need no changes.
fn from_single_env() -> Option<ServerConfig> {
    let token = std::env::var("TELEGRAM_BOT_TOKEN").ok()?;
    let name = std::env::var("TELEGRAM_AGENT_NAME").ok();
    let id = name
        .as_deref()
        .and_then(|n| AgentId::parse(n).ok())
        .unwrap_or_else(|| AgentId::parse("default").expect("literal is valid"));

    Some(ServerConfig {
        agents: vec![AgentConfig {
            profile: AgentSelfProfile {
                name,
                model: std::env::var("TELEGRAM_AGENT_MODEL").ok(),
                description: std::env::var("TELEGRAM_AGENT_DESCRIPTION").ok(),
            },
            id,
            token,
        }],
        primary: None,
        http_addr: std::env::var("TELEGRAM_MCP_HTTP_ADDR").ok(),
        idle_shutdown_secs: std::env::var("TELEGRAM_IDLE_SHUTDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok()),
    })
}

/// Directory holding the config file when the user has not said where it is.
///
/// One place per machine, so the same `agents.toml` serves every project —
/// the alternative, a file per repository, means the agents only exist in
/// whichever directory the client happened to be launched from.
pub fn config_dir() -> Option<PathBuf> {
    let dirs = config_dir_candidates();
    // An existing directory wins over the platform default, so a config
    // written on one OS keeps working after moving to another — and so we
    // never quietly start reading a second, empty location.
    dirs.iter()
        .find(|d| d.is_dir())
        .or_else(|| dirs.first())
        .cloned()
}

/// Config directories in preference order. `~/.config` is included on Windows
/// too: it is where most cross-platform CLI tools put things, so it is often
/// already there.
fn config_dir_candidates() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |base: Option<PathBuf>| {
        if let Some(base) = base {
            let dir = base.join("telegram-agent-mcp");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    };
    if cfg!(windows) {
        push(std::env::var_os("APPDATA").map(PathBuf::from));
    }
    push(std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from));
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    push(home.map(|h| h.join(".config")));
    dirs
}

/// Where `load` looks when given no path, in order.
///
/// The machine-wide file comes before the working directory. The working
/// directory is whichever project the MCP client happened to be launched in,
/// which nobody chose deliberately — picking up a stray `agents.toml` from it
/// would run somebody else's bots.
pub fn candidate_config_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = config_dir_candidates()
        .into_iter()
        .map(|d| d.join("agents.toml"))
        .collect();
    paths.push(PathBuf::from("telegram-agents.toml"));
    paths.push(PathBuf::from("agents.toml"));
    paths
}

/// The first well-known config file that actually exists.
pub fn default_config_path() -> Option<PathBuf> {
    candidate_config_paths().into_iter().find(|p| p.is_file())
}

/// Human-readable note about where the configuration came from, for logs and
/// `--doctor`. Knowing this answers most "why is it running the wrong bot"
/// questions on its own.
pub fn describe_source(explicit_path: &Option<PathBuf>) -> String {
    if let Some(p) = explicit_file(explicit_path.clone()) {
        return format!("config file {}", p.display());
    }
    if std::env::var_os("TELEGRAM_AGENT_1_TOKEN").is_some() {
        return "TELEGRAM_AGENT_<N>_* environment variables".to_string();
    }
    if std::env::var_os("TELEGRAM_BOT_TOKEN").is_some() {
        return "TELEGRAM_BOT_TOKEN".to_string();
    }
    match default_config_path() {
        Some(p) => format!("config file {}", p.display()),
        None => "nothing found".to_string(),
    }
}

/// A config file the caller named outright, by flag or environment.
fn explicit_file(explicit_path: Option<PathBuf>) -> Option<PathBuf> {
    explicit_path.or_else(|| std::env::var("TELEGRAM_AGENTS_FILE").ok().map(Into::into))
}

/// The config file that will actually be used, if any.
pub fn resolve_config_path(explicit_path: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit_file(explicit_path) {
        return Some(p);
    }
    // Tokens in the environment were put there by whoever launched this
    // process — usually pasted straight into an MCP client's config — so they
    // outrank a file nobody pointed at during this run.
    if std::env::var_os("TELEGRAM_AGENT_1_TOKEN").is_some()
        || std::env::var_os("TELEGRAM_BOT_TOKEN").is_some()
    {
        return None;
    }
    default_config_path()
}

/// Loads the configuration, in this order:
///
/// 1. a file named outright (`--config PATH`, `TELEGRAM_AGENTS_FILE`),
/// 2. tokens in the environment,
/// 3. a file in a well-known location.
///
/// Environment beats the well-known file so that pasting a token into an MCP
/// client's config just works, rather than being silently overruled by a file
/// left over from something else.
pub fn load(explicit_path: Option<PathBuf>) -> Result<ServerConfig> {
    let config = match resolve_config_path(explicit_path) {
        Some(p) => from_file(&p)?,
        None => from_numbered_env()
            .or_else(from_single_env)
            .with_context(no_config_help)?,
    };
    config.validate()?;
    Ok(config)
}

/// The error a first-time user is most likely to hit, so it says exactly what
/// to write and where, rather than naming the flag that was missing.
fn no_config_help() -> String {
    let path = config_dir()
        .map(|d| d.join("agents.toml"))
        .unwrap_or_else(|| PathBuf::from("agents.toml"));
    format!(
        "no bot tokens configured.\n\nCreate {} with one entry per bot:\n\n\
         [[agents]]\nname = \"planner\"\ntoken = \"123456:AA...\"   # from @BotFather\n\
         model = \"...\"\n\nThen point each MCP client at `telegram-agent-mcp --agent planner`.",
        path.display()
    )
}

/// Adds one agent to the config file, creating it if it does not exist yet,
/// without disturbing anything else already there.
///
/// This exists because "several agents" otherwise means hand-writing TOML:
/// getting the `[[agents]]` table syntax, the quoting, and where it goes
/// relative to an existing `[server]` block all right is exactly the kind of
/// setup step the single-agent path (paste one token into an env var) does
/// not require — which makes it the actual barrier to two MCP clients (e.g.
/// two opencode windows) getting their own bot identity instead of
/// colliding on one. Appending a single well-formed block, with the
/// validation `ServerConfig` already does, removes that barrier without
/// requiring a TOML *writer* dependency: this only ever adds a block, never
/// rewrites one, so plain string formatting is enough and nothing already in
/// the file — including comments — is touched.
pub fn add_agent(
    explicit_path: Option<PathBuf>,
    name: &str,
    token: &str,
    model: Option<&str>,
    description: Option<&str>,
) -> Result<PathBuf> {
    let id = AgentId::parse(name)?;
    let token = token.trim();
    if !token.contains(':') || token.split(':').next().is_none_or(|n| n.is_empty()) {
        bail!(
            "{token:?} doesn't look like a Telegram bot token — it should look like \
             123456:AA... (copy it from @BotFather)"
        );
    }

    let path = explicit_file(explicit_path)
        .or_else(default_config_path)
        .unwrap_or_else(|| {
            config_dir()
                .map(|d| d.join("agents.toml"))
                .unwrap_or_else(|| PathBuf::from("agents.toml"))
        });

    if path.is_file() {
        let existing = from_file(&path)?;
        if let Some(dup) = existing.find(&id) {
            bail!(
                "agent \"{id}\" is already in {} (token starting {}...). Remove it there first, \
                 or pick a different name.",
                path.display(),
                dup.token.chars().take(7).collect::<String>()
            );
        }
        if let Some(taken_by) = existing.agents.iter().find(|a| a.token == token) {
            bail!(
                "that token is already used by agent {:?} in {}. Telegram allows only one \
                 poller per token — create a separate bot per agent via @BotFather instead of \
                 reusing this one.",
                taken_by.id.to_string(),
                path.display()
            );
        }
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut block = format!(
        "\n[[agents]]\nname = {:?}\ntoken = {:?}\n",
        id.to_string(),
        token
    );
    if let Some(m) = model {
        block.push_str(&format!("model = {m:?}\n"));
    }
    if let Some(d) = description {
        block.push_str(&format!("description = {d:?}\n"));
    }

    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?
        .write_all(block.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;

    // Catches a bug in the block built above now, as a clear error, rather
    // than as a parse failure the next time any agent tries to start.
    from_file(&path).with_context(|| {
        format!(
            "{} no longer parses after adding this agent — this is a bug in telegram-agent-mcp, \
             please report it",
            path.display()
        )
    })?;

    Ok(path)
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// stdio MCP, one agent, own poller. The default and the original
    /// behaviour.
    Stdio,
    /// HTTP MCP server with N agents and N pollers sharing one log.
    Hub {
        addr: String,
        /// Exit once no bridge client has been seen for this long. `None`
        /// (the default for a hand-run `--hub`) means run forever — set only
        /// by [`crate::autostart`] for a hub it started on the caller's
        /// behalf, since that one has no operator watching it who would
        /// otherwise have to remember to stop it.
        idle_shutdown_secs: Option<u64>,
    },
    /// stdio MCP that forwards to a hub over HTTP, for clients that can only
    /// spawn a command.
    Bridge {
        url: String,
        agent: Option<String>,
        /// Start a hub first if nothing is listening. Set when the URL was
        /// defaulted rather than given, so `--agent NAME` on its own is the
        /// whole setup: no separate terminal, no process to remember.
        autostart: bool,
    },
    /// Report what is configured and what works, then exit.
    Doctor,
    /// Appends one agent to the config file and exits. The plug-and-play path
    /// to a second (or third...) identity without hand-writing TOML.
    AddAgent {
        name: String,
        token: String,
        model: Option<String>,
        description: Option<String>,
    },
}

/// The hub endpoint assumed when `--agent` is used without `--connect`.
pub fn default_hub_url() -> String {
    format!("http://{DEFAULT_HTTP_ADDR}/mcp")
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub mode: Mode,
    pub config_path: Option<PathBuf>,
}

pub const USAGE: &str = "\
telegram-agent-mcp — an MCP server exposing Telegram to LLM agents

USAGE:
    telegram-agent-mcp --agent NAME       run as that agent (starts a hub if
                                          one is not already running)
    telegram-agent-mcp                    run the only configured agent
    telegram-agent-mcp --doctor           check the setup and report
    telegram-agent-mcp --add-agent NAME --token TOKEN
                                          add another agent identity, e.g. so
                                          two MCP clients don't collide on one

SETUP:
    One agent (one MCP client, one bot): paste the token into TELEGRAM_BOT_TOKEN
    in that client's config and you're done.

    Two or more (e.g. two opencode windows, each its own identity): create a
    bot per agent with @BotFather, add them all to one group, then run

           telegram-agent-mcp --add-agent planner --token 123456:AA...
           telegram-agent-mcp --add-agent reviewer --token 789012:BB...

    once each (this writes agents.toml — see --doctor for the exact path),
    and point each MCP client at:

           telegram-agent-mcp --agent planner
           telegram-agent-mcp --agent reviewer

    They will not clash: each name gets its own bot token, so the transcript
    and other agents can tell them apart, and `wait_for_reply` won't mistake
    one for the other's own messages.

OPTIONS:
    --agent NAME      Which agent to run as. With more than one configured,
                      this starts (or joins) a shared hub so the agents can
                      see each other's messages.
    --add-agent NAME --token TOKEN [--model M] [--description D]
                      Appends this agent to the config file (creating it if
                      needed) and exits. Rejects a name or token already in
                      use, since either means two agents would collide.
    --doctor          Report the config file in use, the agents in it, whether
                      each token works, and whether a hub is running.
    --config PATH     TOML file listing agents. Also TELEGRAM_AGENTS_FILE.
    --hub [ADDR]      Run only the hub, in the foreground (default
                      127.0.0.1:8787). Useful for watching its logs. Runs
                      forever unless --idle-shutdown is also given.
    --idle-shutdown SECS
                      Exit once no bridge client has been seen for SECS
                      seconds. Set automatically on a hub this process
                      autostarted on the caller's behalf; not set by default
                      on a hand-run --hub, which is assumed to have an
                      operator watching it.
    --connect URL     Bridge stdio to a hub at a specific URL, instead of the
                      default one.
    -h, --help        Show this message.
    -V, --version     Print the version. Worth checking when a local build and
                      an installed wheel are both on the machine.

ENVIRONMENT:
    TELEGRAM_BOT_TOKEN         single-agent token
    TELEGRAM_AGENT_NAME/_MODEL/_DESCRIPTION
                               how that agent announces itself
    TELEGRAM_AGENT_<N>_TOKEN   numbered form for several agents
    TELEGRAM_AGENTS_FILE       path to the TOML config
    TELEGRAM_MCP_HTTP_ADDR     implies --hub at this address
";

pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Cli> {
    let mut mode: Option<Mode> = None;
    let mut config_path = None;
    let mut agent = None;
    let mut idle_shutdown_secs = None;
    let mut add_agent_token = None;
    let mut add_agent_model = None;
    let mut add_agent_description = None;

    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--hub" => {
                // The address is optional, so only consume the next argument
                // if it is not another flag.
                let takes_addr = it.peek().is_some_and(|a| !a.starts_with('-'));
                let addr = if takes_addr {
                    it.next().expect("peeked")
                } else {
                    DEFAULT_HTTP_ADDR.to_string()
                };
                mode = Some(Mode::Hub {
                    addr,
                    idle_shutdown_secs: None,
                });
            }
            "--idle-shutdown" => {
                let raw = it
                    .next()
                    .context("--idle-shutdown needs a number of seconds")?;
                idle_shutdown_secs = Some(raw.parse::<u64>().with_context(|| {
                    format!("--idle-shutdown {raw:?} is not a number of seconds")
                })?);
            }
            "--connect" => {
                let url = it.next().context("--connect needs a URL")?;
                mode = Some(Mode::Bridge {
                    url,
                    agent: None,
                    autostart: false,
                });
            }
            "--doctor" | "--check" => mode = Some(Mode::Doctor),
            "--agent" => agent = Some(it.next().context("--agent needs a name")?),
            "--add-agent" => {
                let name = it.next().context("--add-agent needs a NAME")?;
                mode = Some(Mode::AddAgent {
                    name,
                    token: String::new(),
                    model: None,
                    description: None,
                });
            }
            "--token" => add_agent_token = Some(it.next().context("--token needs a value")?),
            "--model" => add_agent_model = Some(it.next().context("--model needs a value")?),
            "--description" => {
                add_agent_description = Some(it.next().context("--description needs a value")?)
            }
            "--config" => {
                config_path = Some(PathBuf::from(it.next().context("--config needs a path")?))
            }
            other => bail!("unrecognised argument {other:?}\n\n{USAGE}"),
        }
    }

    // `--agent` and `--idle-shutdown` are parsed independently of order, so
    // apply them at the end.
    if let Some(Mode::Bridge { url, autostart, .. }) = mode.clone() {
        mode = Some(Mode::Bridge {
            url,
            agent: agent.clone(),
            autostart,
        });
    }
    if let Some(Mode::Hub { addr, .. }) = mode.clone() {
        mode = Some(Mode::Hub {
            addr,
            idle_shutdown_secs,
        });
    }
    if let Some(Mode::AddAgent { name, .. }) = mode.clone() {
        let token = add_agent_token.context("--add-agent also needs --token TOKEN")?;
        mode = Some(Mode::AddAgent {
            name,
            token,
            model: add_agent_model.clone(),
            description: add_agent_description.clone(),
        });
    }

    let mode = mode.unwrap_or_else(|| match (agent, std::env::var("TELEGRAM_MCP_HTTP_ADDR")) {
        // `--agent NAME` with no endpoint is the one-line setup: connect to
        // the usual hub, starting it if it is not up yet.
        (Some(name), _) => Mode::Bridge {
            url: default_hub_url(),
            agent: Some(name),
            autostart: true,
        },
        (None, Ok(addr)) => Mode::Hub {
            addr,
            idle_shutdown_secs,
        },
        (None, Err(_)) => Mode::Stdio,
    });

    Ok(Cli { mode, config_path })
}

pub fn parse_addr(addr: &str) -> Result<SocketAddr> {
    addr.parse()
        .with_context(|| format!("{addr:?} is not a valid listen address (host:port)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_stdio() {
        // The published wheel is invoked with no arguments; this must stay
        // the default forever.
        assert_eq!(parse_args(args(&[])).unwrap().mode, Mode::Stdio);
    }

    #[test]
    fn hub_address_is_optional() {
        assert_eq!(
            parse_args(args(&["--hub"])).unwrap().mode,
            Mode::Hub {
                addr: DEFAULT_HTTP_ADDR.into(),
                idle_shutdown_secs: None,
            }
        );
        assert_eq!(
            parse_args(args(&["--hub", "0.0.0.0:9000"])).unwrap().mode,
            Mode::Hub {
                addr: "0.0.0.0:9000".into(),
                idle_shutdown_secs: None,
            }
        );
        // A following flag must not be eaten as the address.
        let cli = parse_args(args(&["--hub", "--config", "a.toml"])).unwrap();
        assert_eq!(
            cli.mode,
            Mode::Hub {
                addr: DEFAULT_HTTP_ADDR.into(),
                idle_shutdown_secs: None,
            }
        );
        assert_eq!(cli.config_path, Some(PathBuf::from("a.toml")));
    }

    #[test]
    fn idle_shutdown_applies_regardless_of_argument_order() {
        assert_eq!(
            parse_args(args(&["--hub", "--idle-shutdown", "300"]))
                .unwrap()
                .mode,
            Mode::Hub {
                addr: DEFAULT_HTTP_ADDR.into(),
                idle_shutdown_secs: Some(300),
            }
        );
        assert_eq!(
            parse_args(args(&["--idle-shutdown", "300", "--hub"]))
                .unwrap()
                .mode,
            Mode::Hub {
                addr: DEFAULT_HTTP_ADDR.into(),
                idle_shutdown_secs: Some(300),
            }
        );
    }

    #[test]
    fn bridge_takes_a_url_and_optional_agent() {
        assert_eq!(
            parse_args(args(&["--connect", "http://localhost:8787/mcp"]))
                .unwrap()
                .mode,
            Mode::Bridge {
                url: "http://localhost:8787/mcp".into(),
                agent: None,
                // An explicit URL names someone else's hub; starting our own
                // there would be wrong.
                autostart: false,
            }
        );
        // --agent may come before or after --connect.
        for a in [
            args(&["--connect", "http://h/mcp", "--agent", "qwen"]),
            args(&["--agent", "qwen", "--connect", "http://h/mcp"]),
        ] {
            assert_eq!(
                parse_args(a).unwrap().mode,
                Mode::Bridge {
                    url: "http://h/mcp".into(),
                    agent: Some("qwen".into()),
                    autostart: false,
                }
            );
        }
    }

    #[test]
    fn a_bare_agent_flag_is_the_whole_setup() {
        // The documented one-liner for an MCP client: no URL, no separately
        // launched hub, no config path.
        assert_eq!(
            parse_args(args(&["--agent", "planner"])).unwrap().mode,
            Mode::Bridge {
                url: default_hub_url(),
                agent: Some("planner".into()),
                autostart: true,
            }
        );
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_args(args(&["--nope"])).is_err());
        assert!(parse_args(args(&["--connect"])).is_err());
    }

    fn cfg(agents: Vec<(&str, &str)>, primary: Option<&str>) -> ServerConfig {
        ServerConfig {
            agents: agents
                .into_iter()
                .map(|(n, t)| AgentConfig {
                    id: AgentId::parse(n).unwrap(),
                    token: t.into(),
                    profile: AgentSelfProfile::default(),
                })
                .collect(),
            primary: primary.map(|p| AgentId::parse(p).unwrap()),
            http_addr: None,
            idle_shutdown_secs: None,
        }
    }

    #[test]
    fn duplicate_tokens_are_rejected_at_startup() {
        // Two pollers on one token 409 each other forever; the runtime error
        // is nowhere near the cause, so catch it here.
        let err = cfg(vec![("a", "tok"), ("b", "tok")], None)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("share one bot token"), "{err}");
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let c = cfg(vec![("a", "t1"), ("a", "t2")], None);
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("more than once")
        );
    }

    #[test]
    fn an_unknown_primary_is_rejected() {
        let c = cfg(vec![("a", "t1")], Some("nope"));
        assert!(c.validate().is_err());
    }

    #[test]
    fn no_agents_is_rejected() {
        assert!(cfg(vec![], None).validate().is_err());
    }

    #[test]
    fn a_lone_agent_is_the_default_but_several_are_ambiguous() {
        let one = cfg(vec![("a", "t1")], None);
        assert_eq!(one.default_agent().unwrap().id.to_string(), "a");

        let two = cfg(vec![("a", "t1"), ("b", "t2")], None);
        assert!(
            two.default_agent().is_none(),
            "with several agents a client must say which one it is"
        );

        let with_primary = cfg(vec![("a", "t1"), ("b", "t2")], Some("b"));
        assert_eq!(with_primary.default_agent().unwrap().id.to_string(), "b");
    }

    #[test]
    fn the_machine_wide_config_is_preferred_over_the_working_directory() {
        // The working directory is whichever project the MCP client was
        // launched in — nobody chose it — so a stray agents.toml there must
        // not outrank the file the user actually wrote.
        let paths = candidate_config_paths();
        let cwd_first = paths
            .iter()
            .position(|p| p.parent() == Some(std::path::Path::new("")));
        let shared_first = paths
            .iter()
            .position(|p| p.parent() != Some(std::path::Path::new("")));
        if let (Some(cwd), Some(shared)) = (cwd_first, shared_first) {
            assert!(shared < cwd, "{paths:?}");
        }
    }

    #[test]
    fn a_config_file_is_parsed() {
        let dir = std::env::temp_dir().join("tam-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agents.toml");
        std::fs::write(
            &path,
            r#"
[server]
http_addr = "127.0.0.1:9999"
primary = "qwen"

[[agents]]
name = "Qwen"
token = "111:aaa"
model = "qwen3-coder"
description = "Rust and systems"

[[agents]]
name = "claude"
token = "222:bbb"
"#,
        )
        .unwrap();

        let c = from_file(&path).unwrap();
        c.validate().unwrap();
        assert_eq!(c.agents.len(), 2);
        // Names are normalised, so ?agent=qwen matches `name = "Qwen"`.
        assert_eq!(c.agents[0].id.to_string(), "qwen");
        assert_eq!(c.agents[0].profile.model.as_deref(), Some("qwen3-coder"));
        assert_eq!(c.http_addr.as_deref(), Some("127.0.0.1:9999"));
        assert_eq!(c.default_agent().unwrap().id.to_string(), "qwen");
        std::fs::remove_file(&path).ok();
    }

    /// A fresh temp path for one test, so parallel test threads never share a
    /// file the way a fixed name like `agents.toml` would risk.
    fn temp_config_path(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tam-add-agent-{test_name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agents.toml");
        std::fs::remove_file(&path).ok();
        path
    }

    #[test]
    fn add_agent_creates_the_file_when_it_does_not_exist_yet() {
        let path = temp_config_path("creates-file");
        let got = add_agent(Some(path.clone()), "planner", "111:aaa", None, None).unwrap();
        assert_eq!(got, path);

        let c = from_file(&path).unwrap();
        assert_eq!(c.agents.len(), 1);
        assert_eq!(c.agents[0].id.to_string(), "planner");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_agent_appends_without_disturbing_what_was_already_there() {
        let path = temp_config_path("appends");
        std::fs::write(
            &path,
            "# a comment a human wrote\n[[agents]]\nname = \"planner\"\ntoken = \"111:aaa\"\n",
        )
        .unwrap();

        add_agent(
            Some(path.clone()),
            "reviewer",
            "222:bbb",
            Some("gpt-5"),
            Some("debugging"),
        )
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# a comment a human wrote"), "{raw}");

        let c = from_file(&path).unwrap();
        assert_eq!(c.agents.len(), 2);
        assert_eq!(c.agents[0].id.to_string(), "planner");
        assert_eq!(c.agents[1].id.to_string(), "reviewer");
        assert_eq!(c.agents[1].profile.model.as_deref(), Some("gpt-5"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_agent_rejects_a_name_already_in_use() {
        let path = temp_config_path("dup-name");
        add_agent(Some(path.clone()), "planner", "111:aaa", None, None).unwrap();

        let err = add_agent(Some(path.clone()), "planner", "222:bbb", None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already in"), "{err}");
        // Must not have appended a second, colliding block.
        assert_eq!(from_file(&path).unwrap().agents.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_agent_rejects_a_token_already_in_use() {
        // Two agents on one token would fight over the same getUpdates
        // poller — the same thing ServerConfig::validate catches at startup,
        // caught here instead so it never gets written down.
        let path = temp_config_path("dup-token");
        add_agent(Some(path.clone()), "planner", "111:aaa", None, None).unwrap();

        let err = add_agent(Some(path.clone()), "reviewer", "111:aaa", None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already used by agent"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_agent_rejects_something_that_is_not_a_bot_token() {
        let path = temp_config_path("bad-token");
        let err = add_agent(Some(path.clone()), "planner", "not-a-token", None, None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("doesn't look like a Telegram bot token"),
            "{err}"
        );
        assert!(!path.exists(), "must not create the file on a rejected add");
    }
}
