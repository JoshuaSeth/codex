use clap::Parser;
use clap::ValueHint;
use codex_core::parse_non_stop_budget;
use codex_core::parse_non_stop_duration;
use codex_utils_cli::ApprovalModeCliArg;
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    version,
    after_long_help = "Persistent terminal mode:\n  - Use `--persistent` to keep the turn alive while any session terminal is still running.\n  - Codex will not accept an assistant-only response as terminal until those terminals exit.\n  - This is opt-in because intentionally leaving a shell open keeps the turn running.\n\nNon-stop mode:\n  - Use `--non-stop` to forbid normal turn completion entirely.\n  - Use `--non-stop-for <DURATION>` to keep that behavior only until the timeout expires; after that, the next normal final answer may stop.\n  - Use `--non-stop-budget <COUNT>` to keep that behavior only until COUNT normal stop attempts have been reached.\n  - Use `/non-stop [on|off|status|<duration>|on <duration>]` to override that behavior at runtime for the current session.\n  - Use `/deep <N>` to arm the next `N` new turns with 4 extra candidate-stop follow-ups before normal stopping resumes.\n  - While a non-stop turn is running, submitted messages open a mode picker: steer now, release after the next normal-stop boundary, or schedule a timed release.\n  - The explicit queue shortcut uses that same picker, defaulting to the next normal-stop boundary option.\n  - Use `/enqueue-in <delay> <message>` for custom timed releases into the active non-stop turn.\n  - Codex keeps sampling until it is externally interrupted, aborted, otherwise forced to stop, or the configured budget is exhausted.\n  - This is stronger than `--persistent` and can intentionally run forever.\n\nVoice mode:\n  - Use `--voice` to pair Codex with the Dispatch browser voice cockpit.\n  - Assistant speech starts streaming once enough visible text exists, not only after the final answer.\n  - Dispatch voice mode can inject finalized live transcripts straight into the running turn, or submit a fresh new turn after the prior one has stopped.\n  - Add `--non-stop` as well if you want voice mode to keep auto-continuing instead of stopping normally.\n\nCompletion gate:\n  - Use `--completion-criteria <TEXT>` to require a secondary judge call before Codex may stop.\n  - The judge sees real session history, returns strict JSON-schema output, and can block stop.\n  - Denied stops inject a continuation prompt. Judge failures fail closed and keep the turn alive.\n\nStrict filesystem scoping:\n  - Use `--strict-dir <DIR>` (repeatable) to restrict reads+writes to explicit roots.\n  - `--strict-dir` implies workspace-write sandbox and disables default writable temp roots (`/tmp`, `$TMPDIR`).\n  - Commands continue to run normally; approval policy still governs escalation."
)]
pub struct Cli {
    /// Optional user prompt to start the session.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,

    /// Optional image(s) to attach to the initial prompt.
    #[arg(long = "image", short = 'i', value_name = "FILE", value_delimiter = ',', num_args = 1..)]
    pub images: Vec<PathBuf>,

    // Internal controls set by the top-level `codex resume` subcommand.
    // These are not exposed as user flags on the base `codex` command.
    #[clap(skip)]
    pub resume_picker: bool,

    #[clap(skip)]
    pub resume_last: bool,

    /// Internal: resume a specific recorded session by id (UUID). Set by the
    /// top-level `codex resume <SESSION_ID>` wrapper; not exposed as a public flag.
    #[clap(skip)]
    pub resume_session_id: Option<String>,

    /// Internal: show all sessions (disables cwd filtering and shows CWD column).
    #[clap(skip)]
    pub resume_show_all: bool,

    // Internal controls set by the top-level `codex fork` subcommand.
    // These are not exposed as user flags on the base `codex` command.
    #[clap(skip)]
    pub fork_picker: bool,

    #[clap(skip)]
    pub fork_last: bool,

    /// Internal: fork a specific recorded session by id (UUID). Set by the
    /// top-level `codex fork <SESSION_ID>` wrapper; not exposed as a public flag.
    #[clap(skip)]
    pub fork_session_id: Option<String>,

    /// Internal: show all sessions (disables cwd filtering and shows CWD column).
    #[clap(skip)]
    pub fork_show_all: bool,

    /// Model the agent should use.
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    /// Convenience flag to select the local open source model provider. Equivalent to -c
    /// model_provider=oss; verifies a local LM Studio or Ollama server is running.
    #[arg(long = "oss", default_value_t = false)]
    pub oss: bool,

    /// Specify which local provider to use (lmstudio or ollama).
    /// If not specified with --oss, will use config default or show selection.
    #[arg(long = "local-provider")]
    pub oss_provider: Option<String>,

    /// Configuration profile from config.toml to specify default options.
    #[arg(long = "profile", short = 'p')]
    pub config_profile: Option<String>,

    /// Select the sandbox policy to use when executing model-generated shell
    /// commands.
    #[arg(long = "sandbox", short = 's')]
    pub sandbox_mode: Option<codex_utils_cli::SandboxModeCliArg>,

    /// Configure when the model requires human approval before executing a command.
    #[arg(long = "ask-for-approval", short = 'a')]
    pub approval_policy: Option<ApprovalModeCliArg>,

    /// Convenience alias for low-friction sandboxed automatic execution (-a on-request, --sandbox workspace-write).
    #[arg(long = "full-auto", default_value_t = false)]
    pub full_auto: bool,

    /// Skip all confirmation prompts and execute commands without sandboxing.
    /// EXTREMELY DANGEROUS. Intended solely for running in environments that are externally sandboxed.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        default_value_t = false,
        conflicts_with_all = ["approval_policy", "full_auto"]
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Tell the agent to use the specified directory as its working root.
    #[clap(long = "cd", short = 'C', value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Resolve an existing git worktree for this branch and use it as the working root.
    #[arg(long = "branch", value_name = "BRANCH")]
    pub git_branch: Option<String>,

    /// Enable live web search. When enabled, the native Responses `web_search` tool is available to the model (no per‑call approval).
    #[arg(long = "search", default_value_t = false)]
    pub web_search: bool,

    /// Additional directories that should be writable alongside the primary workspace.
    #[arg(long = "add-dir", value_name = "DIR", value_hint = ValueHint::DirPath)]
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
        value_hint = ValueHint::DirPath
    )]
    pub strict_dir: Vec<PathBuf>,

    /// Keep the turn alive while any session terminal is still running.
    ///
    /// This is intended for long-running background terminal workflows where a
    /// short assistant status update must not be treated as the end of the
    /// turn. As long as a session terminal remains alive, Codex keeps
    /// following up instead of accepting a final answer.
    #[arg(long = "persistent", default_value_t = false)]
    pub persistent: bool,

    /// Never allow normal turn completion.
    ///
    /// This is stronger than `--persistent`: even if there are no live
    /// terminals and the model emits a normal final answer, Codex keeps
    /// sampling until the run is externally interrupted, aborted, or errors.
    #[arg(long = "non-stop", default_value_t = false)]
    pub non_stop: bool,

    /// Enable continuous voice mode.
    ///
    /// This is intended to be paired with the Dispatch browser voice cockpit,
    /// which handles streamed TTS playback and AssemblyAI live transcription.
    /// Combine with `--non-stop` when you want voice mode to keep
    /// auto-continuing instead of stopping normally.
    #[arg(long = "voice", default_value_t = false)]
    pub voice: bool,

    /// Keep non-stop mode active only until this timeout expires.
    ///
    /// Plain numbers mean minutes; suffix with `s`, `m`, or `h`.
    #[arg(
        long = "non-stop-for",
        value_name = "DURATION",
        value_parser = parse_non_stop_duration,
        conflicts_with = "non_stop"
    )]
    pub non_stop_for: Option<Duration>,

    /// Keep non-stop mode active until this many normal stop attempts.
    #[arg(
        long = "non-stop-budget",
        value_name = "COUNT",
        value_parser = parse_non_stop_budget
    )]
    pub non_stop_budget: Option<u32>,

    /// Require a secondary completion judge before Codex may stop.
    #[arg(long = "completion-criteria")]
    pub completion_criteria: Option<String>,

    /// Read completion criteria text from a file.
    #[arg(
        long = "completion-criteria-file",
        value_name = "FILE",
        conflicts_with = "completion_criteria"
    )]
    pub completion_criteria_file: Option<PathBuf>,

    /// Optional judge model override for the completion gate.
    #[arg(long = "completion-judge-model")]
    pub completion_judge_model: Option<String>,

    /// Optional base URL override for the completion judge provider.
    #[arg(long = "completion-judge-base-url")]
    pub completion_judge_base_url: Option<String>,

    /// Optional environment-variable name containing the completion judge API key.
    #[arg(long = "completion-judge-api-key-env")]
    pub completion_judge_api_key_env: Option<String>,

    /// Completion judge request timeout in milliseconds.
    #[arg(long = "completion-judge-timeout-ms")]
    pub completion_judge_timeout_ms: Option<u64>,

    /// Maximum number of completion judge attempts before failing closed.
    #[arg(long = "completion-judge-max-retries")]
    pub completion_judge_max_retries: Option<u32>,

    /// Maximum assistant messages to include in the completion judge window.
    #[arg(long = "completion-judge-max-assistant-messages")]
    pub completion_judge_max_assistant_messages: Option<usize>,

    /// Maximum user messages to include in the completion judge window.
    #[arg(long = "completion-judge-max-user-messages")]
    pub completion_judge_max_user_messages: Option<usize>,

    /// Disable alternate screen mode
    ///
    /// Runs the TUI in inline mode, preserving terminal scrollback history. This is useful
    /// in terminal multiplexers like Zellij that follow the xterm spec strictly and disable
    /// scrollback in alternate screen buffers.
    #[arg(long = "no-alt-screen", default_value_t = false)]
    pub no_alt_screen: bool,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,
}
