//! Start tm-daemon for a desktop app and stop it with the app.
//!
//! The daemon serves the built web app and the API on a free port of this machine. The
//! launcher reads the address and the pairing code it prints, pairs once, and hands the
//! app the address and a token, so the window opens signed in. Dropping [`Daemon`] stops
//! the daemon and everything it started.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

/// How long the daemon may take to say where it is.
const START_WITHIN: Duration = Duration::from_secs(30);

/// A running daemon. Dropping it stops the daemon.
pub struct Daemon {
    child: Child,
    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub url: String,
    pub token: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        stop(&mut self.child);
    }
}

impl Daemon {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Whether the daemon is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Before an update restarts the app: hand the daemon's running builders to the next
    /// daemon (H3) and let this one go. Their agents keep working in the holder, and the
    /// daemon the restarted app starts picks them up. Returns whether the daemon handed off;
    /// when it did not in time, it is stopped with everything it started, as on drop.
    pub fn hand_off(mut self) -> bool {
        hand_off(&mut self.child, Duration::from_secs(10))
    }
}

/// The status a daemon exits with once it has handed off (`EX_TEMPFAIL`).
pub const HANDED_OFF: i32 = 75;

/// Ask a daemon to hand off (SIGUSR1) and wait up to `within` for it to exit.
fn hand_off(child: &mut Child, within: Duration) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: a signal to our own child, whose handle we still hold.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGUSR1);
        }
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return status.code() == Some(HANDED_OFF);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = (child, within);
    false
}

/// What to start.
pub struct Launch<'a> {
    /// The tm-daemon program (the app's sidecar).
    pub program: &'a Path,
    /// The project it opens first. Created when missing.
    pub project: &'a Path,
    /// The built web app, served at `/`.
    pub web: Option<&'a Path>,
    /// A port to try first, so the app's address (and what the window keeps for it, such
    /// as the theme) is the same every launch. Taken ports fall back to any free one.
    pub port: Option<u16>,
}

/// Start the daemon, wait for its address and code, and pair.
pub fn start(launch: &Launch<'_>) -> anyhow::Result<Daemon> {
    std::fs::create_dir_all(launch.project).with_context(|| format!("cannot create {}", launch.project.display()))?;
    let port = launch.port.filter(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(0);
    let mut cmd = Command::new(launch.program);
    cmd.arg("serve").arg(launch.project).args(["--bind", &format!("127.0.0.1:{port}")]);
    // The daemon keeps its settings and logs under HOME. Windows names that folder
    // USERPROFILE and usually leaves HOME unset.
    if std::env::var_os("HOME").is_none()
        && let Some(profile) = std::env::var_os("USERPROFILE")
    {
        cmd.env("HOME", profile);
    }
    if let Some(web) = launch.web {
        cmd.arg("--web").arg(web);
    }
    let (mut child, line) = spawn_and_read(cmd, START_WITHIN, |line| line.contains(" code "))?;
    let (url, code) = match parse_banner(&line) {
        Some(found) => found,
        None => {
            stop(&mut child);
            bail!("tm-daemon said something unexpected: {line}");
        }
    };
    let token = match pair(&url, &code) {
        Ok(token) => token,
        Err(err) => {
            stop(&mut child);
            return Err(err);
        }
    };
    Ok(Daemon { child, url, token })
}

/// Start `cmd` in its own process group and return the first line `wanted` accepts.
/// Output after that keeps being read, so the daemon never blocks on a full pipe.
pub fn spawn_and_read(mut cmd: Command, within: Duration, wanted: impl Fn(&str) -> bool + Send + 'static) -> anyhow::Result<(Child, String)> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    // A console program started from a window app would open a console window of its own.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("could not start tm-daemon")?;
    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|p| Box::new(p) as _)].into_iter().flatten() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let deadline = std::time::Instant::now() + within;
    let mut said = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if wanted(&line) => {
                // Keep draining in the background.
                std::thread::spawn(move || while rx.recv().is_ok() {});
                return Ok((child, line));
            }
            Ok(line) => said.push(line),
            Err(_) => {
                stop(&mut child);
                let tail = said.iter().rev().take(6).rev().cloned().collect::<Vec<_>>().join("\n");
                bail!("tm-daemon did not start: {}", if tail.is_empty() { "it printed nothing".to_string() } else { tail });
            }
        }
    }
}

/// `taskmaster: api http://127.0.0.1:52341/  code ABCD2345` -> (url, code).
pub fn parse_banner(line: &str) -> Option<(String, String)> {
    let mut words = line.split_whitespace();
    let mut url = None;
    let mut code = None;
    while let Some(word) = words.next() {
        match word {
            "api" => url = words.next().map(|u| u.trim_end_matches('/').to_string()),
            "code" => code = words.next().map(str::to_string),
            _ => {}
        }
    }
    Some((url.filter(|u| u.starts_with("http://"))?, code.filter(|c| !c.is_empty())?))
}

/// Trade the pairing code for a token.
pub fn pair(url: &str, code: &str) -> anyhow::Result<String> {
    let reply: serde_json::Value = ureq::post(&format!("{url}/api/pair"))
        .timeout(Duration::from_secs(10))
        .send_json(serde_json::json!({ "code": code }))
        .context("could not pair with tm-daemon")?
        .into_json()?;
    reply["token"].as_str().map(str::to_string).context("tm-daemon gave no token")
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Stop a daemon and everything it started: TERM to its group, then KILL. On Windows the
/// daemon's process tree is ended; `child` still holds its handle, so the id cannot have
/// been reused by another process.
pub fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as libc::pid_t;
        // SAFETY: signals only; a stale group fails with ESRCH.
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        for _ in 0..30 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if matches!(child.try_wait(), Ok(None)) {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The project a fresh install opens: `~/Taskmaster`.
pub fn default_project() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Taskmaster")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_daemon_asked_to_hand_off_exits_with_the_status_that_says_it_did() {
        let mut child = Command::new("sh").args(["-c", "trap 'exit 75' USR1; while :; do sleep 0.05; done"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(hand_off(&mut child, Duration::from_secs(5)));
        // One that ignores the request is not taken for handed off.
        let mut deaf = Command::new("sh").args(["-c", "trap '' USR1; sleep 5"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!hand_off(&mut deaf, Duration::from_millis(300)));
        stop(&mut deaf);
    }

    #[test]
    fn the_banner_gives_the_address_and_the_code() {
        assert_eq!(
            parse_banner("taskmaster: api http://127.0.0.1:52341/  code ABCD2345"),
            Some(("http://127.0.0.1:52341".into(), "ABCD2345".into()))
        );
        assert_eq!(parse_banner("taskmaster: listening"), None);
        assert_eq!(parse_banner("taskmaster: api ftp://x code 1"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_started_program_is_read_then_stopped_with_its_children() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo warming up; sleep 300 & echo \"$!\" > \"$0\"; echo 'taskmaster: api http://127.0.0.1:9/  code XYZ'; wait", "/tmp/tm-launcher-grandchild"]);
        let (mut child, line) = spawn_and_read(cmd, Duration::from_secs(5), |l| l.contains(" code ")).unwrap();
        assert!(line.contains("code XYZ"));
        std::thread::sleep(Duration::from_millis(100));
        let grandchild: i32 = std::fs::read_to_string("/tmp/tm-launcher-grandchild").unwrap().trim().parse().unwrap();
        stop(&mut child);
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 only checks that the process exists.
        assert_ne!(unsafe { libc::kill(grandchild, 0) }, 0, "the daemon's children stop with it");
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_never_says_where_it_is_is_stopped_with_its_last_words() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo cannot bind; sleep 30"]);
        let err = spawn_and_read(cmd, Duration::from_millis(500), |l| l.contains(" code ")).err().unwrap().to_string();
        assert!(err.contains("cannot bind"), "{err}");
    }

    /// The real daemon, when the workspace has built it: start, pair, health, stop.
    #[test]
    fn the_real_daemon_starts_pairs_and_stops() {
        let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/tm-daemon");
        if !program.is_file() {
            eprintln!("tm-daemon is not built; skipped");
            return;
        }
        let project = std::env::temp_dir().join(format!("tm-launcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        let mut daemon = start(&Launch { program: &program, project: &project, web: None, port: None }).unwrap();
        assert!(daemon.url.starts_with("http://127.0.0.1:"));
        assert_eq!(daemon.token.len(), 64, "a full token");
        let runs = ureq::get(&format!("{}/api/runs", daemon.url)).set("authorization", &format!("Bearer {}", daemon.token)).call().unwrap();
        assert_eq!(runs.status(), 200);
        assert!(daemon.alive());
        let url = daemon.url.clone();
        drop(daemon);
        assert!(ureq::get(&format!("{url}/api/health")).timeout(Duration::from_secs(2)).call().is_err(), "stopped with the app");
        std::fs::remove_dir_all(project).ok();
    }
}
