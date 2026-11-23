//! An agent's process, started here or by the holder (H3), and its lines in and out.
//!
//! Every adapter reads its agent line by line and writes to it line by line, so the same
//! handle serves a child of this process and an agent the holder owns. A held agent is
//! started by the holder when the task names one, and here when the holder cannot be reached.

use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

use crate::hold::{HoldSpec, Logged};
use crate::{Freed, GroupGuard};

/// The agent's output, and its input when it takes one.
pub enum AgentIo {
    Local { lines: Lines<BufReader<ChildStdout>>, stdin: Option<ChildStdin> },
    #[cfg(unix)]
    Held(crate::hold::HeldIo),
}

impl AgentIo {
    /// The next line the agent printed; `None` once its output is closed. Safe to cancel.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        match self {
            AgentIo::Local { lines, .. } => lines.next_line().await,
            #[cfg(unix)]
            AgentIo::Held(held) => held.next_line().await,
        }
    }

    /// Write one line to the agent's input.
    pub async fn write_line(&mut self, line: &str) -> io::Result<()> {
        match self {
            AgentIo::Local { stdin, .. } => {
                let stdin = stdin.as_mut().ok_or_else(|| io::Error::other("the agent takes no input"))?;
                stdin.write_all(line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            }
            #[cfg(unix)]
            AgentIo::Held(held) => held.write_line(line).await,
        }
    }

    /// Whether the agent is inside a turn. A held agent kept open between turns is not
    /// handed off to the next daemon.
    pub fn set_in_turn(&self, on: bool) {
        match self {
            AgentIo::Local { .. } => {}
            #[cfg(unix)]
            AgentIo::Held(held) => held.set_in_turn(on),
        }
    }

    /// Whether the holder owns this agent.
    pub fn is_held(&self) -> bool {
        !matches!(self, AgentIo::Local { .. })
    }
}

/// What frees the agent when its turn is over.
pub enum AgentProc {
    Local { child: tokio::process::Child, group: GroupGuard },
    #[cfg(unix)]
    Held { holder: crate::hold::Holder, key: String },
}

impl AgentProc {
    /// Free the agent and everything it started, and say what that freed.
    pub async fn release(self) -> Freed {
        match self {
            AgentProc::Local { mut child, group } => group.release(&mut child).await,
            #[cfg(unix)]
            AgentProc::Held { holder, key } => holder.kill(&key).await.unwrap_or_default(),
        }
    }
}

/// Start `cmd` with its output piped, and its input with `stdin`. With a [`HoldSpec`] the
/// holder starts it, so it can outlive this daemon; when the holder cannot be reached it
/// starts here, as before.
pub async fn start(cmd: &mut Command, stdin: bool, hold: Option<&HoldSpec>) -> io::Result<(AgentProc, AgentIo)> {
    #[cfg(unix)]
    if let Some(spec) = hold {
        match spec.holder.spawn(&spec.key, cmd, stdin, spec.meta.clone()).await {
            Ok(held) => {
                let proc = AgentProc::Held { holder: spec.holder.clone(), key: spec.key.clone() };
                return Ok((proc, AgentIo::Held(held)));
            }
            Err(err) => eprintln!("taskmaster: the holder did not start {} ({err}); starting it here", spec.key),
        }
    }
    #[cfg(not(unix))]
    let _ = hold;
    cmd.stdin(if stdin { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::null());
    let (mut child, group) = crate::spawn_grouped(cmd)?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("the agent's output was not captured"))?;
    let input = child.stdin.take();
    if stdin && input.is_none() {
        return Err(io::Error::other("the agent's input was not captured"));
    }
    Ok((AgentProc::Local { child, group }, AgentIo::Local { lines: BufReader::new(stdout).lines(), stdin: input }))
}

/// An agent the holder kept through a daemon update, taken over by this daemon: its
/// connection, which reads on where the last daemon stopped, and the log up to there.
pub struct Attached {
    pub io: AgentIo,
    pub proc: AgentProc,
    pub history: Vec<Logged>,
    /// The turn was started to keep its session open afterwards (`run_kept`).
    pub keep: bool,
}

impl Attached {
    /// What the agent printed that the last daemon already read, in order.
    pub fn outputs(&self) -> impl Iterator<Item = &str> {
        self.history.iter().filter_map(|entry| match entry {
            Logged::Out(line) => Some(line.as_str()),
            Logged::In(_) => None,
        })
    }
}

/// Take over the agent the holder kept under `spec`'s key.
#[cfg(unix)]
pub async fn attach(spec: &HoldSpec, keep: bool) -> io::Result<Attached> {
    let (io, history) = spec.holder.attach(&spec.key).await?;
    Ok(Attached {
        io: AgentIo::Held(io),
        proc: AgentProc::Held { holder: spec.holder.clone(), key: spec.key.clone() },
        history,
        keep,
    })
}

#[cfg(not(unix))]
pub async fn attach(_spec: &HoldSpec, _keep: bool) -> io::Result<Attached> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the holder needs a Unix system"))
}
