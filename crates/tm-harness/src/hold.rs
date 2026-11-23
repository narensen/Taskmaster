//! H3: agents that outlive a daemon update.
//!
//! The holder is a small process (`tm-daemon hold <socket>`) that starts builder agents for
//! the daemon and owns their pipes. It keeps, in order, every line each agent printed and every
//! line written to it. A daemon that is being replaced stops reading at a line boundary and
//! tells the holder how far it got (`keep`); the holder keeps those agents running, and their
//! output, until a new daemon attaches. The new daemon gets the log, rebuilds the adapter's
//! state from the part the old one already handled (sending nothing again), and reads on from
//! there.
//!
//! An agent still dies with the connection that owns it, as it did with the daemon: a daemon
//! that stops, crashes or drops a call takes its agents with it. Only a hand-off keeps them, and
//! only for [`KEEP_FOR`]. The socket lives in a directory only this user can open, and the
//! holder refuses a peer of another user.
//!
//! The wire is JSON lines. A connection's first line is a request (`ping`, `list`, `keep`,
//! `kill`, `spawn`, `attach`) and its first reply line says `ok`. After `spawn` or `attach` the
//! connection owns the agent: the holder sends `{"out": line}` for each line the agent prints
//! and `{"exit": code}` when it is gone, and the client sends `{"in": line}` to write to it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

/// The protocol a holder speaks. A client refuses a holder that speaks another.
pub const VERSION: u64 = 1;

/// How long a holder keeps an agent that was handed off, waiting for a new daemon.
pub const KEEP_FOR: Duration = Duration::from_secs(600);

/// A holder with no agents and no connections for this long exits, so the next one is started
/// from the program as it is then.
const IDLE_EXIT: Duration = Duration::from_secs(60);

/// The log of one agent stops growing past this many bytes. Such an agent cannot be picked up
/// again; at a hand-off it goes the usual way (H2 resumes its session).
const LOG_CAP: usize = 64 << 20;

/// What a worker task carries so its agent is started by the holder.
#[derive(Clone)]
pub struct HoldSpec {
    pub holder: Holder,
    /// Unique among the holder's agents: project, run and agent.
    pub key: String,
    /// What a new daemon needs to pick the agent up again. The holder keeps it as it is.
    pub meta: Value,
}

impl std::fmt::Debug for HoldSpec {
    // The meta carries the agent's Dictionary token: it is not printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HoldSpec").field("key", &self.key).finish_non_exhaustive()
    }
}

/// One line of an agent's log: what it printed (`Out`) or what was written to it (`In`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Logged {
    Out(String),
    In(String),
}

/// An agent the holder has, as `list` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct HeldInfo {
    pub key: String,
    pub pid: u32,
    #[serde(default)]
    pub meta: Value,
    /// Lines of its output the last owner had read when it was handed off.
    #[serde(default)]
    pub consumed: u64,
    /// Lines of output so far.
    #[serde(default)]
    pub outs: u64,
    #[serde(default)]
    pub exited: bool,
    /// A connection owns it now.
    #[serde(default)]
    pub owned: bool,
    /// It was handed off and is waiting for a new owner.
    #[serde(default)]
    pub kept: bool,
}

/// Where this user's holder listens: `$TASKMASTER_HOLD` (`off` turns the holder off), else
/// `$XDG_RUNTIME_DIR/taskmaster/hold.sock`, else `~/.local/state/taskmaster/hold.sock`.
pub fn default_socket() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("TASKMASTER_HOLD") {
        let text = value.to_string_lossy();
        if matches!(text.trim(), "off" | "0" | "false" | "no") {
            return None;
        }
        if !text.trim().is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("taskmaster/hold.sock"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/taskmaster/hold.sock"))
}

/// A daemon's handle on the holder. Cheap to clone; every clone shares the hand-off state.
#[derive(Clone)]
pub struct Holder(Arc<Inner>);

struct Inner {
    socket: PathBuf,
    /// How to start a holder when none answers: `<program> hold <socket>`. `None` only connects.
    program: Option<PathBuf>,
    /// Set by a hand-off: readers stop at a line boundary.
    frozen: tokio::sync::watch::Sender<bool>,
    handing_off: AtomicBool,
    /// The agents this daemon reads now, by key: how far each has been read.
    tracked: Mutex<HashMap<String, Arc<Tracked>>>,
    starting: tokio::sync::Mutex<()>,
    /// A holder that would not start is not waited for again until then: agents start here.
    down_until: Mutex<Option<Instant>>,
}

#[cfg_attr(not(unix), allow(dead_code))]
struct Tracked {
    consumed: AtomicU64,
    /// Inside a turn. An agent kept open between turns is not handed off.
    in_turn: AtomicBool,
}

impl Holder {
    /// A handle on the holder at `socket`. With a `program`, a holder is started when none answers.
    pub fn new(socket: impl Into<PathBuf>, program: Option<PathBuf>) -> Holder {
        Holder(Arc::new(Inner {
            socket: socket.into(),
            program,
            frozen: tokio::sync::watch::channel(false).0,
            handing_off: AtomicBool::new(false),
            tracked: Mutex::new(HashMap::new()),
            starting: tokio::sync::Mutex::new(()),
            down_until: Mutex::new(None),
        }))
    }

    pub fn socket(&self) -> &Path {
        &self.0.socket
    }

    /// The daemon is handing its agents off: what it tears down must leave theirs in place.
    pub fn handing_off(&self) -> bool {
        self.0.handing_off.load(Ordering::SeqCst)
    }

    /// Stop reading from every held agent at a line boundary, give what was read `settle` to
    /// reach the transcript, and ask the holder to keep the agents that are inside a turn for
    /// `keep`. Returns how many it keeps. Readers stay stopped: this daemon is on its way out.
    pub async fn hand_off(&self, keep: Duration, settle: Duration) -> std::io::Result<usize> {
        self.0.handing_off.store(true, Ordering::SeqCst);
        self.0.frozen.send_replace(true);
        tokio::time::sleep(settle).await;
        let items: Vec<Value> = self
            .0
            .tracked
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t)| t.in_turn.load(Ordering::SeqCst))
            .map(|(key, t)| json!({ "key": key, "consumed": t.consumed.load(Ordering::SeqCst) }))
            .collect();
        if items.is_empty() {
            return Ok(0);
        }
        let reply = self.call(json!({ "op": "keep", "items": items, "secs": keep.as_secs() }), false).await?;
        Ok(reply["kept"].as_u64().unwrap_or(0) as usize)
    }

    /// Wait while a hand-off has stopped reading. Pending forever once it has.
    #[cfg_attr(not(unix), allow(dead_code))]
    async fn readable(&self) {
        let mut rx = self.0.frozen.subscribe();
        let _ = rx.wait_for(|frozen| !frozen).await;
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn track(&self, key: &str, consumed: u64) -> Arc<Tracked> {
        let tracked = Arc::new(Tracked { consumed: AtomicU64::new(consumed), in_turn: AtomicBool::new(true) });
        self.0.tracked.lock().unwrap().insert(key.to_string(), tracked.clone());
        tracked
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn untrack(&self, key: &str, tracked: &Arc<Tracked>) {
        let mut map = self.0.tracked.lock().unwrap();
        if map.get(key).is_some_and(|have| Arc::ptr_eq(have, tracked)) {
            map.remove(key);
        }
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;

    use std::io;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::mpsc;

    fn bad(text: impl Into<String>) -> io::Error {
        io::Error::other(text.into())
    }

    impl Holder {
        async fn connect(&self) -> io::Result<UnixStream> {
            UnixStream::connect(&self.0.socket).await
        }

        /// Connect, starting a holder when none answers and this handle knows how.
        async fn connect_or_start(&self) -> io::Result<UnixStream> {
            if let Ok(stream) = self.connect().await {
                return Ok(stream);
            }
            let Some(program) = self.0.program.clone() else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no holder is running"));
            };
            let _one = self.0.starting.lock().await;
            if let Ok(stream) = self.connect().await {
                return Ok(stream);
            }
            if self.0.down_until.lock().unwrap().is_some_and(|until| until > Instant::now()) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "the holder would not start a moment ago"));
            }
            // A Unix socket's path has room for about a hundred bytes.
            if self.0.socket.as_os_str().len() > 100 {
                *self.0.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(3600));
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "the holder's socket path is too long; set TASKMASTER_HOLD"));
            }
            // Under systemd it goes in a scope of its own, or stopping the service stops it
            // too; when that does not work, it is started directly.
            let systemd = std::env::var_os("INVOCATION_ID").is_some() && crate::sandbox::resolve_binary("systemd-run").is_ok();
            let ways: &[(bool, u64)] = if systemd { &[(true, 2), (false, 3)] } else { &[(false, 5)] };
            let mut last = io::Error::new(io::ErrorKind::NotFound, "the holder did not start");
            for &(scoped, secs) in ways {
                if let Err(err) = start_holder(&program, &self.0.socket, scoped) {
                    last = err;
                    continue;
                }
                let deadline = Instant::now() + Duration::from_secs(secs);
                loop {
                    match self.connect().await {
                        Ok(stream) => return Ok(stream),
                        Err(err) if Instant::now() >= deadline => {
                            last = err;
                            break;
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                    }
                }
            }
            *self.0.down_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(60));
            Err(last)
        }

        /// Send one request on a new connection and read its reply line.
        async fn open(&self, request: Value, start: bool) -> io::Result<(Value, BufReader<OwnedReadHalf>, OwnedWriteHalf)> {
            let stream = if start { self.connect_or_start().await? } else { self.connect().await? };
            let (read, mut write) = stream.into_split();
            let mut line = request.to_string();
            line.push('\n');
            write.write_all(line.as_bytes()).await?;
            let mut reader = BufReader::new(read);
            let mut buf = Vec::new();
            if reader.read_until(b'\n', &mut buf).await? == 0 {
                return Err(bad("the holder closed the connection"));
            }
            let reply: Value = serde_json::from_slice(&buf).map_err(|err| bad(format!("the holder's reply is not JSON: {err}")))?;
            if reply["ok"] != true {
                return Err(bad(reply["error"].as_str().unwrap_or("the holder refused").to_string()));
            }
            Ok((reply, reader, write))
        }

        pub(super) async fn call(&self, request: Value, start: bool) -> io::Result<Value> {
            self.open(request, start).await.map(|(reply, _, _)| reply)
        }

        /// The protocol version of the holder that answers, starting one if needed.
        pub async fn ping(&self) -> io::Result<u64> {
            Ok(self.call(json!({ "op": "ping" }), true).await?["version"].as_u64().unwrap_or(0))
        }

        /// The agents the holder has. Empty, not an error, when no holder is running.
        pub async fn list(&self) -> io::Result<Vec<HeldInfo>> {
            let reply = match self.call(json!({ "op": "list" }), false).await {
                Ok(reply) => reply,
                Err(err) if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => return Ok(Vec::new()),
                Err(err) => return Err(err),
            };
            if reply["version"].as_u64() != Some(VERSION) {
                return Ok(Vec::new());
            }
            Ok(serde_json::from_value(reply["agents"].clone()).unwrap_or_default())
        }

        /// Kill an agent and everything it started, and forget it.
        pub async fn kill(&self, key: &str) -> io::Result<crate::Freed> {
            let reply = self.call(json!({ "op": "kill", "key": key }), false).await?;
            let freed = &reply["freed"];
            Ok(crate::Freed {
                processes: freed["processes"].as_u64().unwrap_or(0) as usize,
                lingering: freed["lingering"].as_u64().unwrap_or(0) as usize,
            })
        }

        /// Start `cmd` in the holder under `key`, with this daemon's environment as the command
        /// would have had it. The connection that comes back owns the agent.
        pub async fn spawn(&self, key: &str, cmd: &tokio::process::Command, stdin: bool, meta: Value) -> io::Result<HeldIo> {
            let std = cmd.as_std();
            let mut env: HashMap<String, String> = std::env::vars_os()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
                .collect();
            for (name, value) in std.get_envs() {
                let name = name.to_string_lossy().into_owned();
                match value {
                    Some(value) => {
                        env.insert(name, value.to_string_lossy().into_owned());
                    }
                    None => {
                        env.remove(&name);
                    }
                }
            }
            for name in crate::parent_session_vars() {
                env.remove(&name);
            }
            let cwd = match std.get_current_dir() {
                Some(dir) => dir.to_path_buf(),
                None => std::env::current_dir()?,
            };
            let request = json!({
                "op": "spawn",
                "key": key,
                "program": std.get_program().to_string_lossy(),
                "args": std.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                "cwd": cwd.to_string_lossy(),
                "env": env,
                "stdin": stdin,
                "meta": meta,
            });
            let (reply, reader, writer) = self.open(request, true).await?;
            let tracked = self.track(key, 0);
            Ok(HeldIo {
                holder: self.clone(),
                key: key.to_string(),
                pid: reply["pid"].as_u64().unwrap_or(0) as u32,
                reader,
                writer,
                buf: Vec::new(),
                pending: VecDeque::new(),
                tracked,
                exit: None,
            })
        }

        /// Take over an agent the holder kept. Returns its connection, which reads on from where
        /// the last owner stopped, and the log up to there: everything written to it, and what it
        /// printed that the last owner had read.
        pub async fn attach(&self, key: &str) -> io::Result<(HeldIo, Vec<Logged>)> {
            let (reply, reader, writer) = self.open(json!({ "op": "attach", "key": key }), false).await?;
            let consumed = reply["consumed"].as_u64().unwrap_or(0);
            let mut history = Vec::new();
            let mut pending = VecDeque::new();
            let mut outs = 0u64;
            for entry in reply["log"].as_array().map(Vec::as_slice).unwrap_or_default() {
                if let Some(line) = entry["o"].as_str() {
                    if outs < consumed {
                        history.push(Logged::Out(line.to_string()));
                    } else {
                        pending.push_back(line.to_string());
                    }
                    outs += 1;
                } else if let Some(line) = entry["i"].as_str() {
                    history.push(Logged::In(line.to_string()));
                }
            }
            let tracked = self.track(key, consumed);
            let io = HeldIo {
                holder: self.clone(),
                key: key.to_string(),
                pid: reply["pid"].as_u64().unwrap_or(0) as u32,
                reader,
                writer,
                buf: Vec::new(),
                pending,
                tracked,
                exit: None,
            };
            Ok((io, history))
        }
    }

    /// Start a holder for `socket`, in its own session so a signal to the daemon's group does
    /// not reach it; with `scoped`, in a systemd scope of its own.
    fn start_holder(program: &Path, socket: &Path, scoped: bool) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        let mut cmd = if scoped {
            let mut cmd = std::process::Command::new("systemd-run");
            cmd.args(["--user", "--scope", "--quiet", "--collect", "--"]).arg(program);
            cmd
        } else {
            std::process::Command::new(program)
        };
        cmd.arg("hold").arg(socket);
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        for name in crate::parent_session_vars() {
            cmd.env_remove(name);
        }
        // SAFETY: setsid only detaches the child from the daemon's session and group.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        // Reap it if it exits while this daemon still runs.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// A daemon's connection to one held agent: its output lines, and a way to write to it.
    pub struct HeldIo {
        holder: Holder,
        key: String,
        pid: u32,
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
        /// A frame read in part: `read_until` keeps what it has here when it is cancelled.
        buf: Vec<u8>,
        /// Output the last owner had not read yet, from the log.
        pending: VecDeque<String>,
        tracked: Arc<Tracked>,
        exit: Option<Option<i32>>,
    }

    impl HeldIo {
        pub fn key(&self) -> &str {
            &self.key
        }

        pub fn pid(&self) -> u32 {
            self.pid
        }

        pub fn holder(&self) -> &Holder {
            &self.holder
        }

        /// Whether the agent is inside a turn: only then is it kept through a hand-off.
        pub fn set_in_turn(&self, on: bool) {
            self.tracked.in_turn.store(on, Ordering::SeqCst);
        }

        /// The next line the agent printed, `None` once it is gone. Safe to cancel. During a
        /// hand-off it waits for good, so no line is read that the next daemon would not.
        pub async fn next_line(&mut self) -> io::Result<Option<String>> {
            self.holder.readable().await;
            if let Some(line) = self.pending.pop_front() {
                self.tracked.consumed.fetch_add(1, Ordering::SeqCst);
                return Ok(Some(line));
            }
            loop {
                if self.exit.is_some() {
                    return Ok(None);
                }
                let read = self.reader.read_until(b'\n', &mut self.buf).await?;
                if read == 0 && self.buf.is_empty() {
                    return Ok(None);
                }
                if !self.buf.ends_with(b"\n") && read != 0 {
                    continue;
                }
                let frame: Value = serde_json::from_slice(&self.buf).unwrap_or(Value::Null);
                self.buf.clear();
                if let Some(line) = frame["out"].as_str() {
                    self.tracked.consumed.fetch_add(1, Ordering::SeqCst);
                    return Ok(Some(line.to_string()));
                }
                if frame.get("exit").is_some() {
                    self.exit = Some(frame["exit"].as_i64().map(|c| c as i32));
                    return Ok(None);
                }
                if read == 0 {
                    return Ok(None);
                }
            }
        }

        /// Write one line to the agent's input. During a hand-off it waits for good: an answer
        /// this daemon would give now (say, a refusal as it shuts down) is the next one's to give.
        pub async fn write_line(&mut self, line: &str) -> io::Result<()> {
            self.holder.readable().await;
            let mut frame = json!({ "in": line }).to_string();
            frame.push('\n');
            self.writer.write_all(frame.as_bytes()).await?;
            self.writer.flush().await
        }
    }

    impl Drop for HeldIo {
        fn drop(&mut self) {
            self.holder.untrack(&self.key, &self.tracked);
        }
    }

    // ---- the holder process -------------------------------------------------------------

    /// One agent the holder runs.
    struct Agent {
        pid: u32,
        meta: Value,
        log: Vec<Logged>,
        bytes: usize,
        overflow: bool,
        outs: u64,
        consumed: u64,
        stdin: Option<mpsc::UnboundedSender<String>>,
        /// The connection that owns it, and where its frames go.
        owner: Option<(u64, mpsc::UnboundedSender<String>)>,
        keep_until: Option<Instant>,
        /// Set once it has exited, with its code when it had one.
        exit: Option<Option<i32>>,
        killed: bool,
    }

    impl Agent {
        fn kept(&self) -> bool {
            self.keep_until.is_some_and(|until| until > Instant::now())
        }

        fn record(&mut self, entry: Logged) {
            if self.overflow {
                return;
            }
            let len = match &entry {
                Logged::Out(line) | Logged::In(line) => line.len(),
            };
            self.bytes += len;
            if self.bytes > LOG_CAP {
                // Too long to replay: drop the log, the agent can only be killed now.
                self.overflow = true;
                self.log = Vec::new();
                return;
            }
            self.log.push(entry);
        }

        fn send(&self, frame: Value) {
            if let Some((_, tx)) = &self.owner {
                let mut line = frame.to_string();
                line.push('\n');
                let _ = tx.send(line);
            }
        }
    }

    type Agents = Mutex<HashMap<String, Arc<Mutex<Agent>>>>;

    struct Shared {
        agents: Agents,
        conns: AtomicUsize,
        next_conn: AtomicU64,
        busy: Mutex<Instant>,
    }

    /// Serve a holder on `listener` until it has been idle for `idle` (never, with `None`).
    /// Every agent still running then is killed.
    pub async fn serve(listener: UnixListener, idle: Option<Duration>) -> io::Result<()> {
        let shared = Arc::new(Shared {
            agents: Mutex::new(HashMap::new()),
            conns: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            busy: Mutex::new(Instant::now()),
        });
        let mut sweep = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    if !same_user(&stream) {
                        continue;
                    }
                    shared.conns.fetch_add(1, Ordering::SeqCst);
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        connection(&shared, stream).await;
                        shared.conns.fetch_sub(1, Ordering::SeqCst);
                        *shared.busy.lock().unwrap() = Instant::now();
                    });
                }
                _ = sweep.tick() => {
                    sweep_agents(&shared);
                    let empty = shared.agents.lock().unwrap().is_empty() && shared.conns.load(Ordering::SeqCst) == 0;
                    if !empty {
                        *shared.busy.lock().unwrap() = Instant::now();
                    } else if idle.is_some_and(|idle| shared.busy.lock().unwrap().elapsed() >= idle) {
                        break;
                    }
                }
            }
        }
        for (_, agent) in shared.agents.lock().unwrap().drain() {
            kill_now(&agent);
        }
        Ok(())
    }

    /// The `tm-daemon hold <socket>` program: one holder per socket, until it is idle.
    pub fn run_holder(socket: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = socket.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        // A directory it makes is this user's alone; one that is there already is left as it is.
        // The peer check below is what keeps other users out either way.
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(socket.with_extension("lock"))?;
        // SAFETY: flock on a file this process opened; the lock is released when it exits.
        let locked = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            // Another holder serves this socket.
            return Ok(());
        }
        let _ = std::fs::remove_file(socket);
        let listener = std::os::unix::net::UnixListener::bind(socket)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let result = runtime.block_on(async move {
            let listener = UnixListener::from_std(listener)?;
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                served = serve(listener, Some(IDLE_EXIT)) => served,
                _ = term.recv() => Ok(()),
            }
        });
        let _ = std::fs::remove_file(socket);
        drop(lock);
        result.map_err(Into::into)
    }

    /// Only a process of the user who runs the holder may talk to it.
    fn same_user(stream: &UnixStream) -> bool {
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        stream.peer_cred().is_ok_and(|cred| cred.uid() == me)
    }

    fn sweep_agents(shared: &Shared) {
        let mut agents = shared.agents.lock().unwrap();
        let gone: Vec<String> = agents
            .iter()
            .filter(|(_, agent)| {
                let agent = agent.lock().unwrap();
                agent.owner.is_none() && !agent.kept()
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            if let Some(agent) = agents.remove(&key) {
                kill_now(&agent);
            }
        }
    }

    fn kill_now(agent: &Arc<Mutex<Agent>>) {
        let mut agent = agent.lock().unwrap();
        if agent.exit.is_none() && !agent.killed {
            agent.killed = true;
            crate::kill_group(agent.pid);
        }
    }

    async fn write_line(write: &mut OwnedWriteHalf, value: Value) {
        let mut line = value.to_string();
        line.push('\n');
        let _ = write.write_all(line.as_bytes()).await;
    }

    async fn connection(shared: &Arc<Shared>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut first = Vec::new();
        if !matches!(reader.read_until(b'\n', &mut first).await, Ok(n) if n > 0) {
            return;
        }
        let Ok(request) = serde_json::from_slice::<Value>(&first) else {
            write_line(&mut write, json!({ "ok": false, "error": "the request is not JSON" })).await;
            return;
        };
        let key = request["key"].as_str().unwrap_or_default().to_string();
        match request["op"].as_str() {
            Some("ping") => write_line(&mut write, json!({ "ok": true, "version": VERSION })).await,
            Some("list") => {
                let agents: Vec<Value> = shared
                    .agents
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(key, agent)| {
                        let a = agent.lock().unwrap();
                        json!({
                            "key": key, "pid": a.pid, "meta": a.meta, "consumed": a.consumed, "outs": a.outs,
                            "exited": a.exit.is_some(), "owned": a.owner.is_some(), "kept": a.kept(),
                        })
                    })
                    .collect();
                write_line(&mut write, json!({ "ok": true, "version": VERSION, "agents": agents })).await;
            }
            Some("keep") => {
                let secs = request["secs"].as_u64().unwrap_or(KEEP_FOR.as_secs());
                let until = Instant::now() + Duration::from_secs(secs);
                let mut kept = 0;
                {
                    let agents = shared.agents.lock().unwrap();
                    for item in request["items"].as_array().map(Vec::as_slice).unwrap_or_default() {
                        let Some(agent) = item["key"].as_str().and_then(|key| agents.get(key)) else { continue };
                        let mut agent = agent.lock().unwrap();
                        if agent.overflow || agent.killed {
                            continue;
                        }
                        agent.consumed = item["consumed"].as_u64().unwrap_or(0).min(agent.outs);
                        agent.keep_until = Some(until);
                        kept += 1;
                    }
                }
                write_line(&mut write, json!({ "ok": true, "kept": kept })).await;
            }
            Some("kill") => {
                let agent = shared.agents.lock().unwrap().remove(&key);
                let freed = match agent {
                    Some(agent) => kill_and_reap(&agent).await,
                    None => crate::Freed::default(),
                };
                write_line(&mut write, json!({ "ok": true, "freed": { "processes": freed.processes, "lingering": freed.lingering } })).await;
            }
            Some("spawn") => match spawn_agent(shared, &key, &request) {
                Ok(agent) => own(shared, agent, false, reader, write).await,
                Err(err) => write_line(&mut write, json!({ "ok": false, "error": format!("could not start the agent: {err}") })).await,
            },
            Some("attach") => {
                let agent = shared.agents.lock().unwrap().get(&key).cloned();
                match agent {
                    Some(agent) if !agent.lock().unwrap().overflow => own(shared, agent, true, reader, write).await,
                    Some(_) => write_line(&mut write, json!({ "ok": false, "error": "the agent's log was too long to replay" })).await,
                    None => write_line(&mut write, json!({ "ok": false, "error": format!("the holder has no agent `{key}`") })).await,
                }
            }
            _ => write_line(&mut write, json!({ "ok": false, "error": "unknown op" })).await,
        }
    }

    fn spawn_agent(shared: &Arc<Shared>, key: &str, request: &Value) -> io::Result<Arc<Mutex<Agent>>> {
        if key.is_empty() {
            return Err(bad("an agent needs a key"));
        }
        // An agent of the same key is a leftover: it goes first.
        if let Some(old) = shared.agents.lock().unwrap().remove(key) {
            kill_now(&old);
        }
        let program = request["program"].as_str().ok_or_else(|| bad("no program"))?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(request["args"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Value::as_str));
        if let Some(cwd) = request["cwd"].as_str() {
            cmd.current_dir(cwd);
        }
        cmd.env_clear();
        if let Some(env) = request["env"].as_object() {
            cmd.envs(env.iter().filter_map(|(k, v)| v.as_str().map(|v| (k, v))));
        }
        let stdin = request["stdin"] == true;
        cmd.stdin(if stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .kill_on_drop(false);
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        let stdout = child.stdout.take().ok_or_else(|| bad("no output"))?;
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let agent = Arc::new(Mutex::new(Agent {
            pid,
            meta: request["meta"].clone(),
            log: Vec::new(),
            bytes: 0,
            overflow: false,
            outs: 0,
            consumed: 0,
            stdin: stdin.then_some(stdin_tx),
            owner: None,
            // Until its connection takes it over, a moment later, the sweeper leaves it alone.
            keep_until: Some(Instant::now() + Duration::from_secs(10)),
            exit: None,
            killed: false,
        }));
        if let Some(mut pipe) = child.stdin.take() {
            tokio::spawn(async move {
                while let Some(mut line) = stdin_rx.recv().await {
                    line.push('\n');
                    if pipe.write_all(line.as_bytes()).await.is_err() || pipe.flush().await.is_err() {
                        break;
                    }
                }
            });
        }
        let eof = Arc::new(tokio::sync::Notify::new());
        let output = agent.clone();
        let at_eof = eof.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    buf.pop();
                }
                let line = String::from_utf8_lossy(&buf).into_owned();
                let mut agent = output.lock().unwrap();
                agent.outs += 1;
                agent.send(json!({ "out": line }));
                agent.record(Logged::Out(line));
            }
            at_eof.notify_one();
        });
        let exited = agent.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // A grandchild may hold the output open: give its last lines a moment, not forever.
            let _ = tokio::time::timeout(Duration::from_secs(1), eof.notified()).await;
            let code = status.ok().and_then(|s| s.code());
            let mut agent = exited.lock().unwrap();
            agent.exit = Some(code);
            agent.send(json!({ "exit": code }));
            // The owner's writer ends with its sender, and the client sees the end.
            agent.owner = agent.owner.take().map(|(id, _)| (id, mpsc::unbounded_channel().0));
        });
        shared.agents.lock().unwrap().insert(key.to_string(), agent.clone());
        Ok(agent)
    }

    /// Serve the connection that owns `agent` (spawned, or `attach`ed), until it closes. When it
    /// closes, the agent is killed unless it was handed off.
    async fn own(shared: &Arc<Shared>, agent: Arc<Mutex<Agent>>, attach: bool, mut reader: BufReader<OwnedReadHalf>, mut write: OwnedWriteHalf) {
        let conn = shared.next_conn.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        {
            let mut a = agent.lock().unwrap();
            let reply = match attach {
                false => json!({ "ok": true, "pid": a.pid }),
                true => {
                    let log: Vec<Value> = a
                        .log
                        .iter()
                        .map(|entry| match entry {
                            Logged::Out(line) => json!({ "o": line }),
                            Logged::In(line) => json!({ "i": line }),
                        })
                        .collect();
                    json!({ "ok": true, "pid": a.pid, "consumed": a.consumed, "log": log, "meta": a.meta })
                }
            };
            let mut line = reply.to_string();
            line.push('\n');
            let _ = tx.send(line);
            // A new owner: the agent is no longer waiting for one.
            a.keep_until = None;
            a.owner = Some((conn, tx));
            if let Some(code) = a.exit {
                a.send(json!({ "exit": code }));
            }
        }
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if !buf.ends_with(b"\n") {
                break;
            }
            let Ok(frame) = serde_json::from_slice::<Value>(&buf) else { continue };
            if let Some(line) = frame["in"].as_str() {
                let mut a = agent.lock().unwrap();
                if a.owner.as_ref().is_some_and(|(id, _)| *id == conn)
                    && let Some(stdin) = &a.stdin
                {
                    let _ = stdin.send(line.to_string());
                    a.record(Logged::In(line.to_string()));
                }
            }
        }
        let gone = {
            let mut a = agent.lock().unwrap();
            if a.owner.as_ref().is_some_and(|(id, _)| *id == conn) {
                a.owner = None;
                !a.kept()
            } else {
                false
            }
        };
        if gone {
            // Its daemon let go without handing it off: it goes, as it would have with the daemon.
            shared.agents.lock().unwrap().retain(|_, have| !Arc::ptr_eq(have, &agent));
            kill_now(&agent);
        }
        writer.abort();
    }

    /// Kill an agent's whole group, wait for it, and count what is left of the group.
    async fn kill_and_reap(agent: &Arc<Mutex<Agent>>) -> crate::Freed {
        let pid = agent.lock().unwrap().pid;
        let processes = crate::group_members(pid);
        kill_now(agent);
        for _ in 0..40 {
            if agent.lock().unwrap().exit.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut lingering = crate::group_members(pid);
        for _ in 0..20 {
            if lingering == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            lingering = crate::group_members(pid);
        }
        crate::Freed { processes, lingering }
    }

    /// A holder served on a tokio task, for tests: its socket in `dir`, never idle-exiting.
    pub fn in_process(dir: &Path) -> io::Result<Holder> {
        std::fs::create_dir_all(dir)?;
        let socket = dir.join("hold.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        tokio::spawn(async move {
            let _ = serve(listener, None).await;
        });
        Ok(Holder::new(socket, None))
    }
}

#[cfg(not(unix))]
mod other {
    use super::*;

    impl Holder {
        pub async fn list(&self) -> std::io::Result<Vec<HeldInfo>> {
            Ok(Vec::new())
        }

        pub async fn kill(&self, _key: &str) -> std::io::Result<crate::Freed> {
            Ok(crate::Freed::default())
        }

        pub(super) async fn call(&self, _request: Value, _start: bool) -> std::io::Result<Value> {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "the holder needs a Unix system"))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tm-hold-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sh(script: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    async fn gone(pid: u32) -> bool {
        for _ in 0..100 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_held_agent_talks_through_the_holder_and_dies_with_its_connection() {
        let holder = in_process(&dir("own")).unwrap();
        let mut io = holder.spawn("p/r/a", &sh("echo ready; read l; echo got:$l; sleep 300"), true, json!({ "run": "r" })).await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("ready"));
        io.write_line("hi").await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("got:hi"));
        let pid = io.pid();
        assert!(alive(pid));
        let listed = holder.list().await.unwrap();
        assert_eq!((listed.len(), listed[0].owned, listed[0].meta["run"].as_str()), (1, true, Some("r")));
        // The daemon let go without a hand-off: the agent goes, as it did with the daemon.
        drop(io);
        assert!(gone(pid).await, "the agent outlived its connection");
        assert!(holder.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_handed_off_agent_keeps_running_and_the_next_daemon_reads_on_where_the_last_stopped() {
        let dir = dir("keep");
        let old = in_process(&dir).unwrap();
        let script = "echo one; read l; echo two:$l; echo three; read m; echo four:$m";
        let mut io = old.spawn("p/r/a", &sh(script), true, json!({})).await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("one"));
        io.write_line("x").await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("two:x"));
        assert_eq!(old.hand_off(Duration::from_secs(60), Duration::from_millis(50)).await.unwrap(), 1);
        // Reading has stopped for good on the old side.
        assert!(tokio::time::timeout(Duration::from_millis(200), io.next_line()).await.is_err());
        let pid = io.pid();
        drop(io);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(alive(pid), "a handed-off agent was killed with its old connection");

        // The next daemon: a new handle on the same socket.
        let new = Holder::new(old.socket(), None);
        let listed = new.list().await.unwrap();
        assert_eq!((listed[0].kept, listed[0].owned, listed[0].consumed), (true, false, 2));
        let (mut io, history) = new.attach("p/r/a").await.unwrap();
        assert_eq!(history, vec![Logged::Out("one".into()), Logged::In("x".into()), Logged::Out("two:x".into())]);
        // What the agent printed after the last read comes first, then the live output.
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("three"));
        io.write_line("y").await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("four:y"));
        assert_eq!(io.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn output_printed_while_no_daemon_reads_is_kept_for_the_next_one() {
        let old = in_process(&dir("gap")).unwrap();
        let mut io = old.spawn("p/r/a", &sh("echo one; read l; echo two; echo three"), true, json!({})).await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("one"));
        io.write_line("go").await.unwrap();
        old.hand_off(Duration::from_secs(60), Duration::from_millis(20)).await.unwrap();
        drop(io);
        // The agent finishes while no daemon is attached.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let new = Holder::new(old.socket(), None);
        let (mut io, history) = new.attach("p/r/a").await.unwrap();
        assert_eq!(history, vec![Logged::Out("one".into()), Logged::In("go".into())]);
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("two"));
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("three"));
        assert_eq!(io.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn killing_a_held_agent_frees_everything_it_started() {
        let holder = in_process(&dir("kill")).unwrap();
        let mut io = holder.spawn("p/r/a", &sh("sleep 300 & sleep 300 & echo go; wait"), false, json!({})).await.unwrap();
        assert_eq!(io.next_line().await.unwrap().as_deref(), Some("go"));
        let freed = holder.kill("p/r/a").await.unwrap();
        assert_eq!(freed, crate::Freed { processes: 3, lingering: 0 });
        assert_eq!(io.next_line().await.unwrap(), None);
        assert!(holder.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_holder_that_will_not_start_is_not_waited_for_twice() {
        // `false hold <socket>` exits at once: no holder ever answers.
        let holder = Holder::new(dir("down").join("hold.sock"), Some(PathBuf::from("/bin/false")));
        assert!(holder.spawn("k", &sh("true"), false, json!({})).await.is_err());
        let again = Instant::now();
        assert!(holder.spawn("k", &sh("true"), false, json!({})).await.is_err());
        assert!(again.elapsed() < Duration::from_millis(500), "the second spawn waited for the holder again");
    }

    #[tokio::test]
    async fn no_holder_lists_nothing_and_a_spawn_without_one_fails_so_the_caller_starts_it_here() {
        let holder = Holder::new(dir("none").join("hold.sock"), None);
        assert!(holder.list().await.unwrap().is_empty());
        assert!(holder.spawn("k", &sh("true"), false, json!({})).await.is_err());
    }
}
