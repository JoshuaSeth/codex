use anyhow::Context;
use codex_exec::exec_events::ThreadEvent;
use serde_json::Value;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::app::ViewInput;

pub(super) fn tail_events_file(
    path: &Path,
    follow: bool,
    tail: bool,
    poll: Duration,
    tx: mpsc::Sender<ViewInput>,
    stop: Arc<AtomicBool>,
) {
    let mut file = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match File::open(path) {
            Ok(file) => break file,
            Err(err) => {
                let _ = tx.send(ViewInput::IoError {
                    message: format!("Failed to open {}: {err}", path.display()),
                });
                if !follow {
                    return;
                }
                thread::sleep(poll);
            }
        }
    };

    if tail {
        let _ = file.seek(SeekFrom::End(0));
    }

    let mut reader = BufReader::new(file);
    let mut pending = String::new();
    let mut at_eof = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => {
                if !follow {
                    return;
                }
                if !at_eof {
                    let _ = tx.send(ViewInput::AtEof);
                    at_eof = true;
                }
                thread::sleep(poll);
                continue;
            }
            Ok(_) => {
                at_eof = false;
                pending.push_str(&buf);
                if !pending.ends_with('\n') {
                    continue;
                }
                let line = pending.trim_end_matches(['\n', '\r']).to_string();
                pending.clear();
                if line.trim().is_empty() {
                    continue;
                }
                match parse_jsonl_line(&line) {
                    Ok(v) => {
                        let _ = tx.send(v);
                    }
                    Err(err) => {
                        let _ = tx.send(ViewInput::InvalidJson);
                        let _ = tx.send(ViewInput::IoError {
                            message: format!("Parse error: {err}"),
                        });
                    }
                }
            }
            Err(err) => {
                let _ = tx.send(ViewInput::IoError {
                    message: format!("Read error {}: {err}", path.display()),
                });
                if !follow {
                    return;
                }
                thread::sleep(poll);
            }
        }
    }
}

fn parse_jsonl_line(line: &str) -> anyhow::Result<ViewInput> {
    let value: Value = serde_json::from_str(line).context("parse json")?;
    match serde_json::from_value::<ThreadEvent>(value) {
        Ok(event) => Ok(ViewInput::ThreadEvent(Box::new(event))),
        Err(_) => Ok(ViewInput::UnknownJson),
    }
}
