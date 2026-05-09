use clap::Args;
use clap::FromArgMatches;
use clap::Parser;
use clap::ValueEnum;
use codex_core::parse_non_stop_budget;
use codex_core::parse_non_stop_duration;
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    version,
    after_long_help = "PitchAI auth policy:\n  - Default: managed shared auth in $CODEX_HOME/auth.json (typically broker-issued).\n  - `CODEX_API_KEY` is never used as implicit fallback.\n  - API-key mode is explicit-only (`CODEX_FORCE_API_KEY_AUTH=1`).\n\nPitchAI automation/broker notes:\n  - Runners can lease auth from auth-token-server via `CODEX_AUTH_BROKER_URL` + `CODEX_AUTH_BROKER_TOKEN`.\n  - On usage/rate limits, the runner can report lease outcome, fetch fresh auth, and auto-continue the same thread.\n\nPersistent terminal mode:\n  - Use `--persistent` to keep the turn alive while any session terminal is still running.\n  - Codex will not accept an assistant-only response as terminal until those terminals exit.\n  - This is opt-in because intentionally leaving a shell open keeps the turn running.\n\nNon-stop mode:\n  - Use `--non-stop` to forbid normal turn completion entirely.\n  - Use `--non-stop-for <DURATION>` to keep that behavior only until the timeout expires; after that, the next normal final answer may stop.\n  - Use `--non-stop-budget <COUNT>` to keep that behavior only until COUNT normal stop attempts have been reached.\n  - Codex keeps sampling until it is externally interrupted, aborted, otherwise forced to stop, or the configured budget is exhausted.\n  - This is stronger than `--persistent` and can intentionally run forever.\n\nCompletion gate:\n  - Use `--completion-criteria <TEXT>` to require a secondary judge call before Codex may stop.\n  - The judge sees real session history, returns strict JSON-schema output, and can block stop.\n  - Denied stops inject a continuation prompt. Judge failures fail closed and keep the turn alive.\n\nStrict filesystem scoping:\n  - Use `--strict-dir <DIR>` (repeatable) to restrict reads+writes to explicit roots.\n  - `--strict-dir` implies workspace-write sandbox and disables default writable temp roots (`/tmp`, `$TMPDIR`).\n  - Commands continue to run normally; approval policy still governs escalation."
)]
pub struct Cli {
    /// Action to perform. If omitted, runs a new non-interactive session.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Optional image(s) to attach to the initial prompt.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1
    )]
    pub images: Vec<PathBuf>,

    /// Model the agent should use.
    #[arg(long, short = 'm', global = true)]
    pub model: Option<String>,

    /// Use open-source provider.
    #[arg(long = "oss", default_value_t = false)]
    pub oss: bool,

    /// Specify which local provider to use (lmstudio or ollama).
    /// If not specified with --oss, will use config default or show selection.
    #[arg(long = "local-provider")]
    pub oss_provider: Option<String>,

    /// Select the sandbox policy to use when executing model-generated shell
    /// commands.
    #[arg(long = "sandbox", short = 's', value_enum)]
    pub sandbox_mode: Option<codex_utils_cli::SandboxModeCliArg>,

    /// Configuration profile from config.toml to specify default options.
    #[arg(long = "profile", short = 'p')]
    pub config_profile: Option<String>,

    /// Convenience alias for low-friction sandboxed automatic execution (-a on-request, --sandbox workspace-write).
    #[arg(long = "full-auto", default_value_t = false, global = true)]
    pub full_auto: bool,

    /// Skip all confirmation prompts and execute commands without sandboxing.
    /// EXTREMELY DANGEROUS. Intended solely for running in environments that are externally sandboxed.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        default_value_t = false,
        global = true,
        conflicts_with = "full_auto"
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Tell the agent to use the specified directory as its working root.
    #[clap(long = "cd", short = 'C', value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Allow running Codex outside a Git repository.
    #[arg(long = "skip-git-repo-check", global = true, default_value_t = false)]
    pub skip_git_repo_check: bool,

    /// Path to a prompt-sequence TOML file describing multiple prompts to run sequentially.
    #[arg(long = "prompt-sequence", value_name = "FILE")]
    pub prompt_sequence: Option<PathBuf>,

    /// Additional directories that should be writable alongside the primary workspace.
    #[arg(long = "add-dir", value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    pub add_dir: Vec<PathBuf>,

    /// Enforce strict filesystem scope for this session.
    ///
    /// Reads and writes are restricted to these directories (plus required
    /// platform read defaults), `/tmp` and `$TMPDIR` are excluded from writable
    /// roots, and `workspace-write` sandbox mode is implied. Shell command
    /// behavior remains unchanged; approvals still apply.
    #[arg(
        long = "strict-dir",
        alias = "restrict-dir",
        value_name = "DIR",
        value_hint = clap::ValueHint::DirPath
    )]
    pub strict_dir: Vec<PathBuf>,

    /// Run without persisting session files to disk.
    #[arg(long = "ephemeral", global = true, default_value_t = false)]
    pub ephemeral: bool,

    /// Keep the turn alive while any session terminal is still running.
    ///
    /// This is intended for long-running background terminal workflows where a
    /// short assistant status update must not be treated as the end of the
    /// turn. As long as a session terminal remains alive, Codex keeps
    /// following up instead of accepting a final answer.
    #[arg(long = "persistent", global = true, default_value_t = false)]
    pub persistent: bool,

    /// Never allow normal turn completion.
    ///
    /// This is stronger than `--persistent`: even if there are no live
    /// terminals and the model emits a normal final answer, Codex keeps
    /// sampling until the run is externally interrupted, aborted, or errors.
    #[arg(long = "non-stop", global = true, default_value_t = false)]
    pub non_stop: bool,

    /// Enable continuous voice mode.
    ///
    /// Assistant speech starts streaming to the Dispatcher voice path as soon
    /// as enough visible text exists. Live user speech is expected to come
    /// from the Dispatch voice cockpit. Combine with `--non-stop` if you want
    /// continuous auto-follow-up sampling.
    #[arg(long = "voice", global = true, default_value_t = false)]
    pub voice: bool,

    /// Keep non-stop mode active only until this timeout expires.
    ///
    /// Plain numbers mean minutes; suffix with `s`, `m`, or `h`.
    #[arg(
        long = "non-stop-for",
        global = true,
        value_name = "DURATION",
        value_parser = parse_non_stop_duration,
        conflicts_with = "non_stop"
    )]
    pub non_stop_for: Option<Duration>,

    /// Keep non-stop mode active until this many normal stop attempts.
    #[arg(
        long = "non-stop-budget",
        global = true,
        value_name = "COUNT",
        value_parser = parse_non_stop_budget
    )]
    pub non_stop_budget: Option<u32>,

    /// Require a secondary completion judge before Codex may stop.
    #[arg(long = "completion-criteria", global = true)]
    pub completion_criteria: Option<String>,

    /// Read completion criteria text from a file.
    #[arg(
        long = "completion-criteria-file",
        global = true,
        value_name = "FILE",
        conflicts_with = "completion_criteria"
    )]
    pub completion_criteria_file: Option<PathBuf>,

    /// Optional judge model override for the completion gate.
    #[arg(long = "completion-judge-model", global = true)]
    pub completion_judge_model: Option<String>,

    /// Optional base URL override for the completion judge provider.
    #[arg(long = "completion-judge-base-url", global = true)]
    pub completion_judge_base_url: Option<String>,

    /// Optional environment-variable name containing the completion judge API key.
    #[arg(long = "completion-judge-api-key-env", global = true)]
    pub completion_judge_api_key_env: Option<String>,

    /// Completion judge request timeout in milliseconds.
    #[arg(long = "completion-judge-timeout-ms", global = true)]
    pub completion_judge_timeout_ms: Option<u64>,

    /// Maximum number of completion judge attempts before failing closed.
    #[arg(long = "completion-judge-max-retries", global = true)]
    pub completion_judge_max_retries: Option<u32>,

    /// Maximum assistant messages to include in the completion judge window.
    #[arg(long = "completion-judge-max-assistant-messages", global = true)]
    pub completion_judge_max_assistant_messages: Option<usize>,

    /// Maximum user messages to include in the completion judge window.
    #[arg(long = "completion-judge-max-user-messages", global = true)]
    pub completion_judge_max_user_messages: Option<usize>,

    /// Path to a JSON Schema file describing the model's final response shape.
    #[arg(long = "output-schema", value_name = "FILE")]
    pub output_schema: Option<PathBuf>,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    /// Specifies color settings for use in the output.
    #[arg(long = "color", value_enum, default_value_t = Color::Auto)]
    pub color: Color,

    /// Force cursor-based progress updates in exec mode.
    #[arg(long = "progress-cursor", default_value_t = false)]
    pub progress_cursor: bool,

    /// Print events to stdout as JSONL.
    #[arg(
        long = "json",
        alias = "experimental-json",
        default_value_t = false,
        global = true
    )]
    pub json: bool,

    /// Specifies file where the last message from the agent should be written.
    #[arg(
        long = "output-last-message",
        short = 'o',
        value_name = "FILE",
        global = true
    )]
    pub last_message_file: Option<PathBuf>,

    /// Initial instructions for the agent. If not provided as an argument (or
    /// if `-` is used), instructions are read from stdin.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Resume a previous session by id or pick the most recent with --last.
    Resume(ResumeArgs),

    /// Run a code review against the current repository.
    Review(ReviewArgs),
}

#[derive(Args, Debug)]
struct ResumeArgsRaw {
    // Note: This is the direct clap shape. We reinterpret the positional when --last is set
    // so "codex resume --last <prompt>" treats the positional as a prompt, not a session id.
    /// Conversation/session selector.
    ///
    /// Accepts a session UUID, a thread name (UUIDs take precedence if it parses), a path to a
    /// `.jsonl` file, or a plain filename (with or without `.jsonl`) under `~/.codex/sessions/**`.
    ///
    /// If omitted, use --last to pick the most recent recorded session.
    #[arg(value_name = "SESSION")]
    session_id: Option<String>,

    /// Resume the most recent recorded session (newest) without specifying an id.
    #[arg(long = "last", default_value_t = false)]
    last: bool,

    /// Show all sessions (disables cwd filtering).
    #[arg(long = "all", default_value_t = false)]
    all: bool,

    /// Fork the selected session into a new session id before resuming.
    /// This preserves the original rollout file so it can be reused later.
    #[arg(long = "fork", default_value_t = false)]
    fork: bool,

    /// Optional image(s) to attach to the prompt sent after resuming.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1
    )]
    images: Vec<PathBuf>,

    /// Prompt to send after resuming the session. If `-` is used, read from stdin.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    prompt: Option<String>,

    /// Resume the session without sending any new user prompt.
    #[arg(long = "no-prompt", default_value_t = false, conflicts_with = "prompt")]
    no_prompt: bool,
}

#[derive(Debug)]
pub struct ResumeArgs {
    /// Conversation/session selector.
    /// If omitted, use --last to pick the most recent recorded session.
    pub session_id: Option<String>,

    /// Resume the most recent recorded session (newest) without specifying an id.
    pub last: bool,

    /// Show all sessions (disables cwd filtering).
    pub all: bool,

    /// Fork the selected session into a new session id before resuming.
    /// This preserves the original rollout file so it can be reused later.
    pub fork: bool,

    /// Optional image(s) to attach to the prompt sent after resuming.
    pub images: Vec<PathBuf>,

    /// Prompt to send after resuming the session. If `-` is used, read from stdin.
    pub prompt: Option<String>,

    /// Resume the session without sending any new user prompt.
    pub no_prompt: bool,
}

impl From<ResumeArgsRaw> for ResumeArgs {
    fn from(raw: ResumeArgsRaw) -> Self {
        // When --last is used without an explicit prompt, treat the positional as the prompt
        // (clap can’t express this conditional positional meaning cleanly).
        let (session_id, prompt) = if raw.no_prompt {
            (raw.session_id, None)
        } else if raw.last && raw.prompt.is_none() {
            (None, raw.session_id)
        } else {
            (raw.session_id, raw.prompt)
        };
        Self {
            session_id,
            last: raw.last,
            all: raw.all,
            fork: raw.fork,
            images: raw.images,
            prompt,
            no_prompt: raw.no_prompt,
        }
    }
}

impl Args for ResumeArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        ResumeArgsRaw::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        ResumeArgsRaw::augment_args_for_update(cmd)
    }
}

impl FromArgMatches for ResumeArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        ResumeArgsRaw::from_arg_matches(matches).map(Self::from)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = ResumeArgsRaw::from_arg_matches(matches).map(Self::from)?;
        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct ReviewArgs {
    /// Review staged, unstaged, and untracked changes.
    #[arg(
        long = "uncommitted",
        default_value_t = false,
        conflicts_with_all = ["base", "commit", "prompt"]
    )]
    pub uncommitted: bool,

    /// Review changes against the given base branch.
    #[arg(
        long = "base",
        value_name = "BRANCH",
        conflicts_with_all = ["uncommitted", "commit", "prompt"]
    )]
    pub base: Option<String>,

    /// Review the changes introduced by a commit.
    #[arg(
        long = "commit",
        value_name = "SHA",
        conflicts_with_all = ["uncommitted", "base", "prompt"]
    )]
    pub commit: Option<String>,

    /// Optional commit title to display in the review summary.
    #[arg(long = "title", value_name = "TITLE", requires = "commit")]
    pub commit_title: Option<String>,

    /// Custom review instructions. If `-` is used, read from stdin.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Color {
    Always,
    Never,
    #[default]
    Auto,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn resume_parses_prompt_after_global_flags() {
        const PROMPT: &str = "echo resume-with-global-flags-after-subcommand";
        let cli = Cli::parse_from([
            "codex-exec",
            "resume",
            "--last",
            "--json",
            "--model",
            "gpt-5.2-codex",
            "--dangerously-bypass-approvals-and-sandbox",
            "--skip-git-repo-check",
            "--ephemeral",
            "--persistent",
            "--non-stop",
            PROMPT,
        ]);

        assert!(cli.ephemeral);
        assert!(cli.persistent);
        assert!(cli.non_stop);
        assert_eq!(cli.non_stop_for, None);
        let Some(Command::Resume(args)) = cli.command else {
            panic!("expected resume command");
        };
        let effective_prompt = args.prompt.clone().or_else(|| {
            if args.last {
                args.session_id.clone()
            } else {
                None
            }
        });
        assert_eq!(effective_prompt.as_deref(), Some(PROMPT));
    }

    #[test]
    fn resume_parses_non_stop_timeout_flag() {
        let cli = Cli::parse_from(["codex-exec", "resume", "--non-stop-for", "2h", "prompt"]);
        assert_eq!(cli.non_stop_for, Some(Duration::from_secs(2 * 60 * 60)));
    }

    #[test]
    fn resume_parses_non_stop_budget_flag() {
        let cli = Cli::parse_from(["codex-exec", "resume", "--non-stop-budget", "300", "prompt"]);
        assert_eq!(cli.non_stop_budget, Some(300));
    }

    #[test]
    fn resume_accepts_output_last_message_flag_after_subcommand() {
        const PROMPT: &str = "echo resume-with-output-file";
        let cli = Cli::parse_from([
            "codex-exec",
            "resume",
            "session-123",
            "-o",
            "/tmp/resume-output.md",
            PROMPT,
        ]);

        assert_eq!(
            cli.last_message_file,
            Some(PathBuf::from("/tmp/resume-output.md"))
        );
        let Some(Command::Resume(args)) = cli.command else {
            panic!("expected resume command");
        };
        assert_eq!(args.session_id.as_deref(), Some("session-123"));
        assert_eq!(args.prompt.as_deref(), Some(PROMPT));
    }
}
