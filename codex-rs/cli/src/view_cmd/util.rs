use clap::Parser;
use codex_exec::Cli as ExecCli;
use serde_json::Value;
use std::ffi::OsString;

pub(super) fn trim_one_line(text: &str) -> String {
    clip_one_line(text, 80)
}

pub(super) fn clip_one_line(text: &str, max_chars: usize) -> String {
    let s = text.trim().replace('\n', " ");
    let it = s.chars();
    if it.clone().count() <= max_chars {
        return s;
    }

    let mut out: String = it.take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub(super) fn strip_shell_launcher_prefix(command: &str) -> &str {
    // On macOS, command execution can be routed through zsh as:
    //   /bin/zsh -lc '<actual command>'
    // For readability, hide that wrapper in the viewer.
    for prefix in ["/bin/zsh", "/usr/bin/zsh", "zsh"] {
        let Some(rest) = command.strip_prefix(prefix) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix("-lc") else {
            continue;
        };
        return rest.trim_start();
    }

    command
}

pub(super) fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

pub(super) fn extract_effective_prompt(exec_args: &[String]) -> Option<String> {
    let mut argv: Vec<OsString> = Vec::with_capacity(exec_args.len() + 1);
    argv.push(OsString::from("codex exec"));
    argv.extend(exec_args.iter().map(OsString::from));

    let parsed = ExecCli::try_parse_from(argv).ok()?;
    match parsed.command {
        Some(codex_exec::Command::Resume(args)) => {
            if args.no_prompt {
                return None;
            }
            args.prompt.or(parsed.prompt)
        }
        _ => parsed.prompt,
    }
}
