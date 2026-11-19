use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum, value_parser};
use color_eyre::Result;
use color_eyre::eyre::bail;
use thiserror::Error;

use maki_agent::HostPrompt;
use maki_agent::tools::{all_builtin_tool_names, is_builtin_tool};
use maki_config::AgentConfig;

const DEFAULT_BACKGROUND_TIMEOUT_SECS: u64 = 600;
/// A week. The deadline is an `Instant`, which panics on overflow.
const MAX_BACKGROUND_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;
const HOST_PROMPT_PATHS: &[RunPath] = &[RunPath::Tui, RunPath::SystemPromptPreview];

/// Where a run goes, for flags only some paths read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunPath {
    Tui,
    SystemPromptPreview,
    Other,
}

impl fmt::Display for RunPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tui => "the TUI",
            Self::SystemPromptPreview => "maki prompt system (without --tools)",
            Self::Other => "this subcommand",
        })
    }
}

struct ScopedFlag {
    name: &'static str,
    set: bool,
    paths: &'static [RunPath],
}

#[derive(Debug, Error)]
#[error("--{flag} has no effect with {path}. It applies to: {supported}")]
pub struct UnsupportedFlag {
    flag: &'static str,
    path: RunPath,
    supported: String,
}

#[derive(Clone, ValueEnum, Default)]
pub enum PromptVariant {
    #[default]
    System,
    Research,
    General,
}

#[derive(Parser)]
#[command(name = "maki", version, about = "AI coding agent for the terminal")]
// Only one way to name the session to load, or the resolver would quietly pick
// one. `--fork-session` requires it, since forking nothing used to start a
// blank session.
#[command(group = ArgGroup::new("loaded").args(["continue_session", "resume"]))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Model spec (provider/model-id). Defaults to last used model, then configured default_model, then auto-detected
    #[arg(short, long)]
    pub model: Option<String>,

    /// Resume the most recent session in this directory
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session by its ID
    #[arg(
        short = 'r',
        long,
        visible_short_alias = 's',
        visible_alias = "session"
    )]
    pub resume: Option<String>,

    /// Skip loading custom commands from .maki/commands, .claude/commands, etc.
    #[arg(long)]
    pub no_commands: bool,

    /// Skip user `init.lua` files (global and project) but keep the Lua
    /// host and every builtin plugin running, so tools and the default
    /// keymap still load. Use this to recover from a broken `init.lua`
    /// or keymap override. Only Lua `init.lua` files are affected;
    /// `permissions.toml`, custom commands, and env files load as usual.
    #[arg(long)]
    pub no_plugins: bool,

    /// Run plugin Lua on the interpreter with full debug info (no native codegen)
    #[arg(long)]
    pub no_jit: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(long, alias = "dangerously-skip-permissions")]
    pub yolo: bool,

    /// Load shared project `.maki` config for this run without asking and
    /// without recording an answer. For containers and CI, where the state
    /// directory is thrown away anyway. Only use it on a project you trust.
    #[arg(long)]
    pub trust: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code) or snake_case.
    #[arg(long, value_delimiter = ',', visible_alias = "allowedTools")]
    pub allowed_tools: Vec<String>,

    /// Disallowed tools (comma-separated).
    #[arg(long, value_delimiter = ',', visible_alias = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Write this run under a chosen session ID, unless one already exists there
    #[arg(long)]
    pub session_id: Option<String>,

    /// Fork the loaded session under a new ID
    #[arg(long, requires = "loaded")]
    pub fork_session: bool,

    /// Replace the main agent's system prompt
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Append text to the main agent's system prompt
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    /// How long the TUI stays up for background tasks once its input ends.
    /// Each result that comes back in time wakes the model for another turn.
    /// Tasks still running after this are stopped. Unset: waits 600 when
    /// --exit-on-done is passed, and runs background tasks off otherwise.
    /// At most a week
    #[arg(
        long,
        value_name = "SECS",
        value_parser = value_parser!(u64).range(..=MAX_BACKGROUND_TIMEOUT_SECS),
    )]
    pub background_timeout: Option<u64>,

    // Accepted but ignored, so Claude Code SDK callers don't break.
    #[arg(long, hide = true)]
    pub fallback_model: Option<String>,
    #[arg(long, hide = true)]
    pub settings: Option<String>,
    #[arg(long, hide = true)]
    pub setting_sources: Option<String>,
    #[arg(long, hide = true)]
    pub add_dir: Option<String>,
    #[arg(long, hide = true)]
    pub strict_mcp_config: bool,
    #[arg(long, hide = true)]
    pub include_hook_events: bool,
    #[arg(long, hide = true)]
    pub mcp_config: Option<String>,
    #[arg(long, hide = true)]
    pub tools: Option<String>,
    #[arg(long, hide = true)]
    pub betas: Option<String>,
    #[arg(long, hide = true)]
    pub max_thinking_tokens: Option<String>,
    #[arg(long, hide = true)]
    pub effort: Option<String>,
    #[arg(long, hide = true)]
    pub json_schema: Option<String>,
    #[arg(long, hide = true)]
    pub max_budget_usd: Option<String>,
    #[arg(long, hide = true)]
    pub thinking: Option<String>,
    #[arg(long, hide = true)]
    pub thinking_display: Option<String>,

    /// Initial prompt (reads stdin if piped)
    #[arg(value_name = "PROMPT")]
    pub initial_prompt: Option<String>,
}

impl Cli {
    /// `--allowed-tools` replaces the allowlist and rejects an unknown name;
    /// `--disallowed-tools` adds to the denylist and skips one.
    pub fn apply_tool_flags(&self, agent: &mut AgentConfig) -> Result<()> {
        if !self.allowed_tools.is_empty() {
            agent.allowed_tools = self
                .allowed_tools
                .iter()
                .map(|t| normalize_tool_name(t))
                .collect::<Result<Vec<_>>>()?;
        }
        agent.disabled_tools.extend(
            self.disallowed_tools
                .iter()
                .filter_map(|t| normalize_tool_name(t).ok()),
        );
        Ok(())
    }

    pub fn host_prompt(&self) -> HostPrompt {
        HostPrompt::new(
            self.system_prompt.as_deref(),
            self.append_system_prompt.as_deref(),
        )
    }

    fn run_path(&self) -> RunPath {
        match self.command {
            Some(Command::Prompt(PromptArgs {
                variant: PromptVariant::System,
                tools: false,
                ..
            })) => RunPath::SystemPromptPreview,
            Some(_) => RunPath::Other,
            None => RunPath::Tui,
        }
    }

    /// The one list of which run paths read which flag. Only flags whose
    /// silent drop changes what the agent does are here. Every visible flag
    /// must pick a side (`every_visible_flag_is_placed`).
    fn scoped_flags(&self) -> [ScopedFlag; 2] {
        let host_prompt = self.host_prompt();
        [
            ScopedFlag {
                name: "system-prompt",
                set: host_prompt.replaces(),
                paths: HOST_PROMPT_PATHS,
            },
            ScopedFlag {
                name: "append-system-prompt",
                set: host_prompt.appends(),
                paths: HOST_PROMPT_PATHS,
            },
        ]
    }

    /// Runs before anything else, so a run path can read these flags as they
    /// are without checking again which path it is on.
    pub fn check_run_path_flags(&self) -> Result<(), UnsupportedFlag> {
        let path = self.run_path();
        let Some(flag) = self
            .scoped_flags()
            .into_iter()
            .find(|flag| flag.set && !flag.paths.contains(&path))
        else {
            return Ok(());
        };
        Err(UnsupportedFlag {
            flag: flag.name,
            path,
            supported: flag
                .paths
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        })
    }

    /// `None` when background tasks are off. The TUI keeps tasks up while the
    /// session stays open, and waits 600s once its input ends.
    pub fn background_wait(&self) -> Option<Duration> {
        let secs = self
            .background_timeout
            .unwrap_or(DEFAULT_BACKGROUND_TIMEOUT_SECS);
        (secs > 0).then(|| Duration::from_secs(secs))
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage API authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List all available models
    Models {
        /// Refetch the models.dev catalog, ignoring its 24h cache
        #[arg(long)]
        refresh: bool,
    },
    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Run the index tool on a file to see how it looks like
    Index { path: String },
    /// Manage MCP server authentication
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Update maki to the latest version
    Update {
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Disable syntax highlighting
        #[arg(long)]
        no_color: bool,
    },
    /// Rollback to the previous version
    Rollback,
    /// Show the rendered system prompt or tool definitions
    Prompt(PromptArgs),
    /// Manage projects that may load automatic shared project configuration
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
}

#[derive(Args)]
pub struct PromptArgs {
    /// Prompt variant: system (default), research, general
    #[arg(value_enum, default_value_t = PromptVariant::System)]
    pub variant: PromptVariant,
    /// Append the plan mode reminder to the system prompt
    #[arg(long)]
    pub plan: bool,
    /// Show tool definitions (JSON) instead of prompt text
    #[arg(long)]
    pub tools: bool,
    /// With --tools: show only tool names, one per line
    #[arg(long, requires = "tools")]
    pub names: bool,
}

#[derive(Subcommand)]
pub enum TrustAction {
    /// Trust a project to load automatic shared .maki configuration
    Add {
        /// Project to trust. Defaults to the current directory
        path: Option<PathBuf>,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Remove a stored project trust decision
    Remove {
        /// Project to remove. Defaults to the current directory
        path: Option<PathBuf>,
    },
    /// List stored project trust decisions
    List,
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// List sessions
    List {
        /// Show sessions from all projects
        #[arg(short, long)]
        global: bool,
        /// Print machine-readable session summaries, one JSON array
        #[arg(long)]
        json: bool,
    },
    /// Delete a session
    Delete {
        /// Session ID (see `maki session list`)
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        force: bool,
    },
    /// Write a session as a portable JSON transcript to stdout
    Export {
        /// Session ID (see `maki session list`)
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
    },
    /// Store a JSON transcript as a new session in this directory and print its ID
    Import {
        /// Transcript file, or stdin when omitted or `-`
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum McpAction {
    /// Authenticate with an MCP server
    Auth {
        /// Server name from config
        server: String,
    },
    /// Remove stored OAuth credentials for an MCP server
    Logout {
        /// Server name from config
        server: String,
    },
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Authenticate with a provider (interactive if no provider specified)
    Login {
        /// Provider slug (e.g. zai, openai, xai). Omit for interactive selection.
        provider: Option<String>,
    },
    /// Remove stored credentials for a provider
    Logout {
        /// Provider slug (e.g. openai, xai)
        provider: String,
    },
    /// Show authentication status for all providers
    Status,
}

fn normalize_tool_name(name: &str) -> Result<String> {
    let mut result = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    if !is_builtin_tool(&result) {
        bail!(
            "unknown tool '{}'. Valid tools: {}",
            name,
            all_builtin_tool_names().join(", ")
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use clap::{ArgAction, CommandFactory};
    use test_case::test_case;

    use super::*;

    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const APPEND: &str = "You review the auth module only.";
    const REPLACE: &str = "You are a reviewer.";
    /// Visible flags `Cli::scoped_flags` leaves out: every run path reads them
    /// where they mean something, or they only shape output.
    const UNGUARDED_FLAGS: &[&str] = &[
        "model",
        "continue",
        "resume",
        "no-commands",
        "no-plugins",
        "no-jit",
        "yolo",
        "trust",
        "exit-on-done",
        "allowed-tools",
        "disallowed-tools",
        "session-id",
        "fork-session",
        "background-timeout",
    ];

    #[test_case("Read", "read")]
    #[test_case("Bash", "bash")]
    fn normalize_tool_name_valid_inputs(input: &str, expected: &str) {
        assert_eq!(normalize_tool_name(input).unwrap(), expected);
    }

    #[test]
    fn normalize_tool_name_rejects_unknown() {
        let result = normalize_tool_name("NonExistentTool");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn normalize_tool_name_multi_edit_rejects_snake_variant() {
        assert!(normalize_tool_name("MultiEdit").is_err());
    }

    /// `--session-id` with `-c` stays legal: continue the latest, but write
    /// under this id.
    #[test_case(&["-c", "-r", SESSION_ID], false ; "two sessions to load")]
    #[test_case(&["-c", "-s", SESSION_ID], false ; "two sessions to load through the short alias")]
    #[test_case(&["-c", "--session", SESSION_ID], false ; "two sessions to load through the long alias")]
    #[test_case(&["--fork-session"], false ; "a fork with nothing to fork")]
    #[test_case(&["--fork-session", "-c"], true ; "a fork of the latest")]
    #[test_case(&["--fork-session", "-r", SESSION_ID], true ; "a fork of a named session")]
    #[test_case(&["-c", "--session-id", SESSION_ID], true ; "a redirected continue")]
    fn session_flag_combinations(args: &[&str], accepted: bool) {
        let argv = std::iter::once("maki").chain(args.iter().copied());

        assert_eq!(Cli::try_parse_from(argv).is_ok(), accepted);
    }

    #[test_case(&["--exit-on-done"], Some(DEFAULT_BACKGROUND_TIMEOUT_SECS) ; "tui_default")]
    #[test_case(&["--background-timeout", "30"], Some(30) ; "tui_set")]
    fn background_timeout_flag(args: &[&str], expected_secs: Option<u64>) {
        let argv = std::iter::once("maki").chain(args.iter().copied());

        let cli = Cli::try_parse_from(argv).unwrap();

        assert_eq!(
            cli.background_wait(),
            expected_secs.map(Duration::from_secs)
        );
    }

    #[test]
    fn background_timeout_past_the_cap_is_refused() {
        let too_long = (MAX_BACKGROUND_TIMEOUT_SECS + 1).to_string();

        assert!(Cli::try_parse_from(["maki", "--background-timeout", &too_long]).is_err());
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("maki").chain(args.iter().copied())).unwrap()
    }

    #[test_case(&["--append-system-prompt", APPEND], None ; "tui_append")]
    #[test_case(&["--append-system-prompt", APPEND, "prompt"], None ; "prompt_append")]
    #[test_case(&["--system-prompt", "", "models"], None ; "empty_is_unset")]
    #[test_case(&["--append-system-prompt", APPEND, "prompt", "research"], Some("append-system-prompt") ; "subagent_prompt_append")]
    #[test_case(&["--system-prompt", REPLACE, "prompt", "--tools"], Some("system-prompt") ; "tools_prompt_replace")]
    fn run_path_flag_check(args: &[&str], rejected: Option<&'static str>) {
        let checked = parse(args).check_run_path_flags();

        assert_eq!(checked.err().map(|e| e.flag), rejected);
    }

    #[test]
    fn every_visible_flag_is_placed() {
        let scoped = parse(&[]).scoped_flags().map(|flag| flag.name);
        let command = Cli::command();
        let flags: Vec<&str> = command
            .get_arguments()
            .filter(|arg| {
                !arg.is_hide_set()
                    && !matches!(arg.get_action(), ArgAction::Help | ArgAction::Version)
            })
            .filter_map(|arg| arg.get_long())
            .collect();
        for flag in &flags {
            assert!(
                scoped.contains(flag) || UNGUARDED_FLAGS.contains(flag),
                "--{flag}: add it to `Cli::scoped_flags` if a run path ignores it, else to UNGUARDED_FLAGS"
            );
        }
        for name in scoped.iter().chain(UNGUARDED_FLAGS) {
            assert!(flags.contains(name), "--{name} is not a flag");
        }
    }
}
