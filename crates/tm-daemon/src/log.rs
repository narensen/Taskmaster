//! One log for this daemon.
//!
//! Lines go to `$XDG_STATE_HOME/taskmaster/daemon.log`, or
//! `~/.local/state/taskmaster/daemon.log` when that variable is unset. The Settings
//! page reads the same file. A terminal the daemon was started from still shows
//! what it prints there; this file is the copy that stays after the terminal is gone.
//!
//! The file is kept under [`MAX_BYTES`]: past that it becomes `daemon.log.1`, replacing
//! the one before, and a new file is started.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static WRITE: Mutex<()> = Mutex::new(());

/// The size the log grows to before it is rotated.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How much of the end of the file [`tail`] reads.
const TAIL_BYTES: u64 = 256 * 1024;

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("taskmaster/daemon.log"))
}

pub fn note(line: &str) {
    let Some(path) = path() else { return };
    let _guard = WRITE.lock().unwrap();
    append(&path, line, MAX_BYTES);
}

fn append(path: &Path, line: &str, max_bytes: u64) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= max_bytes) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    let _ = writeln!(file, "{} {line}", crate::time::rfc3339_now());
}

/// The last `max_lines` lines, and the path they were read from. Only the end of the file
/// is read, however large it has grown.
pub fn tail(max_lines: usize) -> (Option<String>, Vec<String>) {
    let Some(path) = path() else { return (None, Vec::new()) };
    let lines = tail_of(&path, max_lines, TAIL_BYTES);
    (Some(path.display().to_string()), lines)
}

fn tail_of(path: &Path, max_lines: usize, window: u64) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(window);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // Reading from the middle of the file starts part way through a line.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_rotates_past_its_size_and_the_tail_reads_only_the_end() {
        let dir = std::env::temp_dir().join(format!("tm-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("daemon.log");
        for n in 0..40 {
            append(&path, &format!("line {n}"), 400);
        }
        let current = std::fs::metadata(&path).unwrap().len();
        assert!(current < 400 + 64, "the current file stays near its limit: {current}");
        let old = std::fs::read_to_string(path.with_extension("log.1")).unwrap();
        assert!(old.contains("line "), "the previous file is kept once");
        let last = tail_of(&path, 3, 120);
        assert_eq!(last.len(), 3);
        assert!(last[2].ends_with("line 39"), "{last:?}");
        assert!(last.iter().all(|l| l.contains(" line ")), "no partial first line: {last:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
