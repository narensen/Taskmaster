//! A program run on a pseudo-terminal, for the CLIs that only show their sign-in screen
//! or their interactive mode to a terminal.
//!
//! The child is the leader of a new session, with the terminal as its controlling one, so
//! it and everything it starts can be killed as one group. Output arrives as raw bytes on
//! a channel; [`crate::screen::Screen`] turns them into text.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::sync::mpsc;

use crate::GroupGuard;

/// Keys a person can send to a terminal program besides plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Up,
    Down,
    Left,
    Right,
    Tab,
    Escape,
    Backspace,
    /// Ctrl-C.
    Interrupt,
}

impl Key {
    pub fn parse(name: &str) -> Option<Key> {
        Some(match name {
            "enter" => Key::Enter,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "tab" => Key::Tab,
            "escape" => Key::Escape,
            "backspace" => Key::Backspace,
            "interrupt" => Key::Interrupt,
            _ => return None,
        })
    }

    pub fn bytes(self) -> &'static [u8] {
        match self {
            Key::Enter => b"\r",
            Key::Up => b"\x1b[A",
            Key::Down => b"\x1b[B",
            Key::Right => b"\x1b[C",
            Key::Left => b"\x1b[D",
            Key::Tab => b"\t",
            Key::Escape => b"\x1b",
            Key::Backspace => b"\x7f",
            Key::Interrupt => b"\x03",
        }
    }
}

/// The writing end of a terminal: what is typed into the program.
#[derive(Clone)]
pub struct PtyWriter(Arc<Mutex<File>>);

impl PtyWriter {
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = self.0.lock().unwrap();
        file.write_all(bytes)?;
        file.flush()
    }

    /// Change the terminal's size. The kernel tells the program with SIGWINCH.
    pub fn resize(&self, rows: u16, cols: u16) -> std::io::Result<()> {
        let file = self.0.lock().unwrap();
        let size = libc::winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ reads one winsize from the pointer; the descriptor is the open master.
        if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A running program on a terminal. Dropping it kills the program's whole group.
pub struct PtyChild {
    pub child: tokio::process::Child,
    pub guard: GroupGuard,
    pub writer: PtyWriter,
    /// Raw output; closed when the program and everything holding the terminal are gone.
    pub output: mpsc::Receiver<Vec<u8>>,
}

/// Start `program` on a new terminal of `rows` by `cols` in `cwd`, with `env` added.
pub fn spawn<I, K, V>(program: &Path, args: &[&str], cwd: &Path, env: I, rows: u16, cols: u16) -> anyhow::Result<PtyChild>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let (master, slave) = open(rows, cols)?;
    let mut cmd = tokio::process::Command::new(program);
    for name in crate::parent_session_vars() {
        cmd.env_remove(name);
    }
    cmd.args(args)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("could not start {}", program.display()))?;
    // The parent's copies of the terminal's far end close here, so a read sees the end
    // once the program exits.
    drop(cmd);
    let guard = GroupGuard(child.id().unwrap_or(0));
    let reader = File::from(master.try_clone()?);
    let writer = PtyWriter(Arc::new(Mutex::new(File::from(master))));
    let (tx, output) = mpsc::channel(256);
    std::thread::spawn(move || read_loop(reader, tx));
    Ok(PtyChild { child, guard, writer, output })
}

fn read_loop(mut reader: File, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            // EIO is how Linux reports that the far end has closed.
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

fn open(rows: u16, cols: u16) -> anyhow::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty writes two descriptors into the pointers given.
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("could not open a terminal");
    }
    // SAFETY: openpty succeeded, so both are open descriptors this process owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // SAFETY: FD_CLOEXEC only keeps the master out of other programs this process starts.
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok((master, slave))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect(mut child: PtyChild) -> String {
        let mut text = Vec::new();
        while let Some(chunk) = child.output.recv().await {
            text.extend(chunk);
        }
        let _ = child.child.wait().await;
        String::from_utf8_lossy(&text).into_owned()
    }

    #[tokio::test]
    async fn a_program_sees_a_terminal_of_the_given_size() {
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c", "test -t 1 && echo tty; stty size"],
            Path::new("/"),
            [("TERM", "dumb")],
            33,
            101,
        )
        .unwrap();
        let text = collect(child).await;
        assert!(text.contains("tty"), "{text:?}");
        assert!(text.contains("33 101"), "{text:?}");
    }

    #[tokio::test]
    async fn typed_text_reaches_the_program() {
        let child = spawn(Path::new("/bin/sh"), &["-c", "read line; echo got:$line"], Path::new("/"), [("TERM", "dumb")], 24, 80).unwrap();
        child.writer.write(b"hello").unwrap();
        child.writer.write(Key::Enter.bytes()).unwrap();
        let text = collect(child).await;
        assert!(text.contains("got:hello"), "{text:?}");
    }

    #[tokio::test]
    async fn a_terminal_can_be_resized() {
        let child = spawn(Path::new("/bin/sh"), &["-c", "read line; stty size"], Path::new("/"), [("TERM", "dumb")], 24, 80).unwrap();
        child.writer.resize(40, 120).unwrap();
        child.writer.write(b"go").unwrap();
        child.writer.write(Key::Enter.bytes()).unwrap();
        let text = collect(child).await;
        assert!(text.contains("40 120"), "{text:?}");
    }

    #[test]
    fn only_named_keys_parse() {
        assert_eq!(Key::parse("down"), Some(Key::Down));
        assert_eq!(Key::parse("rm -rf"), None);
    }
}
