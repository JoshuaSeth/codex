//! Support for `-c key=value` overrides shared across Codex CLI tools.
//!
//! This module provides a [`CliConfigOverrides`] struct that can be embedded
//! into a `clap`-derived CLI struct using `#[clap(flatten)]`. Each occurrence
//! of `-c key=value` (or `--config key=value`) will be collected as a raw
//! string. Helper methods are provided to convert the raw strings into
//! key/value pairs as well as to apply them onto a mutable
//! `serde_json::Value` representing the configuration tree.

use clap::ArgAction;
use clap::Parser;
use codex_core::config::set_codex_home_override;
use codex_core::config::set_config_file_override;
use serde::de::Error as SerdeError;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use toml::Value;

/// CLI option that captures arbitrary configuration overrides specified as
/// `-c key=value`. It intentionally keeps both halves **unparsed** so that the
/// calling code can decide how to interpret the right-hand side.
#[derive(Parser, Debug, Default, Clone)]
pub struct CliConfigOverrides {
    /// Override a configuration value that would otherwise be loaded from
    /// `~/.codex/config.toml`. Use a dotted path (`foo.bar.baz`) to override
    /// nested values. The `value` portion is parsed as TOML. If it fails to
    /// parse as TOML, the raw string is used as a literal.
    ///
    /// Examples:
    ///   - `-c model="o3"`
    ///   - `-c 'sandbox_permissions=["disk-full-read-access"]'`
    ///   - `-c shell_environment_policy.inherit=all`
    #[arg(
        short = 'c',
        long = "config",
        value_name = "key=value",
        action = ArgAction::Append,
        global = true,
    )]
    pub raw_overrides: Vec<String>,

    /// Override the Codex config directory (`CODEX_HOME`) for this invocation.
    #[arg(
        long = "config-home",
        value_name = "DIR",
        global = true,
        help = "Load config/auth/logs from DIR instead of $CODEX_HOME (~/.codex by default)"
    )]
    pub config_home: Option<PathBuf>,

    /// Load configuration from an explicit TOML file regardless of codex home.
    #[arg(
        long = "config-file",
        value_name = "FILE",
        global = true,
        help = "Use FILE instead of config.toml (can be outside of $CODEX_HOME)"
    )]
    pub config_file: Option<PathBuf>,
}

impl CliConfigOverrides {
    /// Parse the raw strings captured from the CLI into a list of `(path,
    /// value)` tuples where `value` is a `serde_json::Value`.
    pub fn parse_overrides(&self) -> Result<Vec<(String, Value)>, String> {
        self.apply_config_location_overrides()?;
        self.raw_overrides
            .iter()
            .map(|s| {
                // Only split on the *first* '=' so values are free to contain
                // the character.
                let mut parts = s.splitn(2, '=');
                let key = match parts.next() {
                    Some(k) => k.trim(),
                    None => return Err("Override missing key".to_string()),
                };
                let value_str = parts
                    .next()
                    .ok_or_else(|| format!("Invalid override (missing '='): {s}"))?
                    .trim();

                if key.is_empty() {
                    return Err(format!("Empty key in override: {s}"));
                }

                // Attempt to parse as TOML. If that fails, treat it as a raw
                // string. This allows convenient usage such as
                // `-c model=o3` without the quotes.
                let value: Value = match parse_toml_value(value_str) {
                    Ok(v) => v,
                    Err(_) => {
                        // Strip leading/trailing quotes if present
                        let trimmed = value_str.trim().trim_matches(|c| c == '"' || c == '\'');
                        Value::String(trimmed.to_string())
                    }
                };

                Ok((key.to_string(), value))
            })
            .collect()
    }

    /// Apply all parsed overrides onto `target`. Intermediate objects will be
    /// created as necessary. Values located at the destination path will be
    /// replaced.
    pub fn apply_on_value(&self, target: &mut Value) -> Result<(), String> {
        let overrides = self.parse_overrides()?;
        for (path, value) in overrides {
            apply_single_override(target, &path, value);
        }
        Ok(())
    }

    /// Merge root-level overrides (e.g., parsed before a subcommand) into this
    /// struct so that downstream parsing sees a single view of the overrides.
    /// Values already set on `self` take precedence.
    pub fn prepend_from(&mut self, other: &CliConfigOverrides) {
        self.raw_overrides.splice(0..0, other.raw_overrides.clone());

        inherit_if_absent(&mut self.config_home, other.config_home.clone());
        inherit_if_absent(&mut self.config_file, other.config_file.clone());
    }

    fn apply_config_location_overrides(&self) -> Result<(), String> {
        if let Some(path) = &self.config_home {
            let normalized = canonicalize_or_absolute(path).map_err(|err| {
                format!(
                    "Failed to resolve --config-home path `{}`: {err}",
                    path.display()
                )
            })?;
            set_codex_home_override(normalized);
        }

        if let Some(path) = &self.config_file {
            let resolved = resolve_config_file_override(path).map_err(|err| {
                format!(
                    "Failed to resolve --config-file path `{}`: {err}",
                    path.display()
                )
            })?;
            set_config_file_override(resolved);
        }

        Ok(())
    }
}

fn canonicalize_or_absolute(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(p) => Ok(p),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                Ok(std::env::current_dir()?.join(path))
            }
        }
        Err(err) => Err(err),
    }
}

fn resolve_config_file_override(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        if path.exists() {
            return std::fs::canonicalize(path);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Config file `{}` does not exist", path.display()),
        ));
    }

    let cwd = env::current_dir()?;
    let mut candidates = vec![cwd.join(path)];
    candidates.push(cwd.join(".codex").join(path));
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".codex").join(path));
    }

    for candidate in candidates {
        if candidate.exists() {
            return std::fs::canonicalize(candidate);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("Config file `{}` was not found", path.display()),
    ))
}

#[cfg(test)]
mod config_file_resolution_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn with_cwd<F: FnOnce()>(dir: &Path, func: F) {
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD_LOCK.lock().expect("cwd lock");

        let original = env::current_dir().expect("current dir");
        env::set_current_dir(dir).expect("set cwd");
        func();
        env::set_current_dir(original).expect("restore cwd");
    }

    #[test]
    fn resolves_relative_config_in_current_dir() {
        let tmp = tempdir().expect("tempdir");
        let cfg = tmp.path().join("local.toml");
        fs::write(&cfg, "model = \"test\"").expect("write");

        with_cwd(tmp.path(), || {
            let resolved = resolve_config_file_override(Path::new("local.toml")).expect("resolved");
            assert_eq!(resolved, fs::canonicalize(&cfg).unwrap());
        });
    }

    #[test]
    fn falls_back_to_dot_codex_folder() {
        let tmp = tempdir().expect("tempdir");
        let dot_codex = tmp.path().join(".codex");
        fs::create_dir_all(&dot_codex).expect("mkdir");
        let cfg = dot_codex.join("profile.toml");
        fs::write(&cfg, "model = \"test\"").expect("write");

        with_cwd(tmp.path(), || {
            let resolved =
                resolve_config_file_override(Path::new("profile.toml")).expect("resolved");
            assert_eq!(resolved, fs::canonicalize(&cfg).unwrap());
        });
    }
}

fn inherit_if_absent<T: Clone>(target: &mut Option<T>, candidate: Option<T>) {
    if target.is_none() {
        *target = candidate;
    }
}

/// Apply a single override onto `root`, creating intermediate objects as
/// necessary.
fn apply_single_override(root: &mut Value, path: &str, value: Value) {
    use toml::value::Table;

    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;

    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;

        if is_last {
            match current {
                Value::Table(tbl) => {
                    tbl.insert((*part).to_string(), value);
                }
                _ => {
                    let mut tbl = Table::new();
                    tbl.insert((*part).to_string(), value);
                    *current = Value::Table(tbl);
                }
            }
            return;
        }

        // Traverse or create intermediate table.
        match current {
            Value::Table(tbl) => {
                current = tbl
                    .entry((*part).to_string())
                    .or_insert_with(|| Value::Table(Table::new()));
            }
            _ => {
                *current = Value::Table(Table::new());
                if let Value::Table(tbl) = current {
                    current = tbl
                        .entry((*part).to_string())
                        .or_insert_with(|| Value::Table(Table::new()));
                }
            }
        }
    }
}

fn parse_toml_value(raw: &str) -> Result<Value, toml::de::Error> {
    let wrapped = format!("_x_ = {raw}");
    let table: toml::Table = toml::from_str(&wrapped)?;
    table
        .get("_x_")
        .cloned()
        .ok_or_else(|| SerdeError::custom("missing sentinel key"))
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_scalar() {
        let v = parse_toml_value("42").expect("parse");
        assert_eq!(v.as_integer(), Some(42));
    }

    #[test]
    fn parses_bool() {
        let true_literal = parse_toml_value("true").expect("parse");
        assert_eq!(true_literal.as_bool(), Some(true));

        let false_literal = parse_toml_value("false").expect("parse");
        assert_eq!(false_literal.as_bool(), Some(false));
    }

    #[test]
    fn fails_on_unquoted_string() {
        assert!(parse_toml_value("hello").is_err());
    }

    #[test]
    fn parses_array() {
        let v = parse_toml_value("[1, 2, 3]").expect("parse");
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn parses_inline_table() {
        let v = parse_toml_value("{a = 1, b = 2}").expect("parse");
        let tbl = v.as_table().expect("table");
        assert_eq!(tbl.get("a").unwrap().as_integer(), Some(1));
        assert_eq!(tbl.get("b").unwrap().as_integer(), Some(2));
    }
}
