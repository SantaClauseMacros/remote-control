//! "Copy Diagnostic Logs" — bundles enough to debug a connection problem
//! without ever including anything sensitive.
//!
//! Deliberately excludes: pairing codes, key material, screen content,
//! clipboard contents. `tracing` never logs those (see the log call sites),
//! so a plain tail of the log files is safe to hand to clipboard/support.

use std::fmt::Write as _;

use rc_common::AppPaths;

use crate::engine::CoreStatus;

/// How much of the most recent log to include, per file. Small enough to
/// paste into a bug report or chat, generous enough to show a session.
const MAX_TAIL_BYTES: usize = 60_000;
/// At most this many of the most-recently-modified log files are included.
const MAX_FILES: usize = 2;

/// Build a plain-text diagnostic bundle: build info + current status + the
/// tail of the most recent log file(s).
pub fn build_report(paths: &AppPaths, status: &CoreStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Remote Control diagnostic log");
    let _ = writeln!(out, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "os: Windows");
    let _ = writeln!(out, "device_id: {}", status.device_id);
    let _ = writeln!(out, "state: {:?}", status.state);
    let _ = writeln!(out, "paired devices: {}", status.paired_count);
    let _ = writeln!(out, "active sessions: {}", status.sessions);
    let _ = writeln!(out, "----------------------------------------");

    let mut files: Vec<_> = std::fs::read_dir(&paths.logs_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((e.path(), modified))
        })
        .collect();
    files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));

    if files.is_empty() {
        let _ = writeln!(out, "(no log files found in {})", paths.logs_dir.display());
        return out;
    }

    for (path, _) in files.into_iter().take(MAX_FILES) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let _ = writeln!(out, "=== {name} (tail) ===");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let tail = tail_str(&text, MAX_TAIL_BYTES);
                out.push_str(tail);
                if !tail.ends_with('\n') {
                    out.push('\n');
                }
            }
            Err(e) => {
                let _ = writeln!(out, "(could not read: {e})");
            }
        }
    }
    out
}

/// The last `max_bytes` of `s`, cut on a char boundary (never mid-UTF8).
pub(crate) fn tail_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}
