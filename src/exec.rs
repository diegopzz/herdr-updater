//! Argv-based subprocess execution with a hard wall-clock deadline.
//!
//! Every external call in this crate goes through here. Two rules, both
//! deliberate:
//!
//! 1. **argv only, never a shell string.** Host names, refs and plugin ids come
//!    from config files and from `herdr`'s own output; interpolating them into
//!    `sh -c` would make a repo named `; rm -rf ~` a code path.
//! 2. **Every call has a deadline.** The whole point of this tool is that it
//!    runs at herdr *startup*. A wedged `git ls-remote` against a dead network
//!    must not hang the terminal you are trying to open. On timeout the child
//!    is killed and the call reports `Timeout`, which callers degrade to
//!    "unknown" rather than "up to date" — the safe direction.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum ExecError {
    /// The child outlived `timeout` and was killed.
    Timeout { secs: u64 },
    /// The binary could not be spawned at all (missing from PATH, no +x, ...).
    Spawn(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Timeout { secs } => write!(f, "timed out after {secs}s"),
            ExecError::Spawn(e) => write!(f, "could not run: {e}"),
        }
    }
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    /// stdout with trailing newline stripped — the shape almost every caller
    /// wants when reading a single value back out of a command.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim_end_matches(['\n', '\r'])
    }
}

/// Run `program` with `args`, killing it after `timeout`.
///
/// stdin is closed (`Stdio::null()`) on purpose: a remote command that decides
/// to prompt gets EOF and dies, instead of blocking until the deadline. That
/// distinction matters for the ssh calls in fleet mode, where an unexpected
/// password prompt would otherwise eat the full timeout on every host.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output, ExecError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ExecError::Spawn(format!("{program}: {e}")))?;

    // Readers must run on their own threads. Waiting on the child first and
    // reading after can deadlock: a child that fills the 64 KB pipe buffer
    // blocks on write while we block on wait, and neither side moves.
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        let _ = out_tx.send(s);
    });
    thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        let _ = err_tx.send(s);
    });

    // Poll rather than block, so the deadline is real. 20 ms keeps a fast
    // command (the common case — `herdr status` is single-digit ms) from
    // paying a visible latency tax while costing nothing measurable.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_default();
                let stderr = err_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_default();
                return Ok(Output {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ExecError::Timeout {
                        secs: timeout.as_secs(),
                    });
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(ExecError::Spawn(format!("{program}: {e}"))),
        }
    }
}

/// Run a command attached to the current terminal. This is reserved for an
/// explicit interactive flow such as Herdr's own install preview; background
/// checks continue to use [`run`] with closed stdin and a deadline.
pub fn run_inherited(program: &str, args: &[&str]) -> Result<i32, ExecError> {
    Command::new(program)
        .args(args)
        .status()
        .map(|status| status.code().unwrap_or(-1))
        .map_err(|e| ExecError::Spawn(format!("{program}: {e}")))
}

/// Run a bounded argv command with a caller-provided stdin payload. This is
/// used for non-secret declarative fleet state; the payload never becomes a
/// process argument or a shell interpolation.
pub fn run_with_input(
    program: &str,
    args: &[&str],
    input: &[u8],
    timeout: Duration,
) -> Result<Output, ExecError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ExecError::Spawn(format!("{program}: {e}")))?;

    let mut in_pipe = child.stdin.take().expect("stdin piped");
    let input = input.to_vec();
    thread::spawn(move || {
        let _ = in_pipe.write_all(&input);
        let _ = in_pipe.flush();
    });
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut value = String::new();
        let _ = out_pipe.read_to_string(&mut value);
        let _ = out_tx.send(value);
    });
    thread::spawn(move || {
        let mut value = String::new();
        let _ = err_pipe.read_to_string(&mut value);
        let _ = err_tx.send(value);
    });

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(Output {
                    code: status.code().unwrap_or(-1),
                    stdout: out_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_default(),
                    stderr: err_rx
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_default(),
                });
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExecError::Timeout {
                    secs: timeout.as_secs(),
                });
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(ExecError::Spawn(format!("{program}: {error}"))),
        }
    }
}

/// True when `program` resolves on PATH. Used for capability probes (`gh`,
/// `curl`, `git`) so a missing tool degrades a feature instead of aborting.
///
/// Resolved in-process and memoised. The earlier version shelled out to
/// `which`, which meant `compare()` spawned two extra processes per plugin, on
/// every thread of a parallel inspection — a fleet host with twenty plugins
/// paid forty process spawns to answer a question whose answer cannot change
/// mid-run. PATH is read once per program and cached for the life of the
/// process.
pub fn have(program: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache
        .lock()
        .ok()
        .and_then(|entries| entries.get(program).copied())
    {
        return known;
    }
    let found = resolve(program).is_some();
    if let Ok(mut entries) = cache.lock() {
        entries.insert(program.to_string(), found);
    }
    found
}

/// The executable `program` names, searched the way the OS would search it.
pub fn resolve(program: &str) -> Option<PathBuf> {
    // An explicit path is not a PATH lookup — HERDR_BIN_PATH routinely carries
    // one, and searching PATH for "/root/.local/bin/herdr" would find nothing.
    if program.contains('/') || (cfg!(windows) && program.contains('\\')) {
        let path = PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(program);
        if executable(&candidate) {
            return Some(candidate);
        }
        // On Windows the extension is part of resolution, and PATHEXT is what
        // decides which ones count.
        if cfg!(windows) {
            let extensions =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            for extension in extensions.split(';').filter(|value| !value.is_empty()) {
                let mut name = program.to_string();
                name.push_str(extension);
                let candidate = directory.join(name);
                if executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn captures_stdout_and_code() {
        let o = run("/bin/sh", &["-c", "echo hi"], Duration::from_secs(5)).unwrap();
        assert!(o.ok());
        assert_eq!(o.trimmed(), "hi");
    }

    #[cfg(unix)]
    #[test]
    fn kills_a_child_that_outlives_the_deadline() {
        let e = run("/bin/sh", &["-c", "sleep 30"], Duration::from_millis(300)).unwrap_err();
        assert!(
            matches!(e, ExecError::Timeout { .. }),
            "expected Timeout, got {e:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_reads_stdin_gets_eof_not_a_hang() {
        // The whole reason stdin is null: this must return, not block.
        let o = run("/bin/sh", &["-c", "cat"], Duration::from_secs(5)).unwrap();
        assert!(o.ok());
        assert_eq!(o.trimmed(), "");
    }

    #[cfg(unix)]
    #[test]
    fn provided_input_is_delivered_without_becoming_an_argument() {
        let o = run_with_input(
            "/bin/sh",
            &["-c", "cat"],
            b"declarative-state\n",
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(o.ok());
        assert_eq!(o.trimmed(), "declarative-state");
    }

    #[cfg(unix)]
    #[test]
    fn resolves_programs_through_path_without_spawning_which() {
        assert!(have("sh"), "sh must resolve on any unix host");
        assert!(!have("herdr-updater-no-such-binary"));
        // Cached answers must stay stable across calls.
        assert!(have("sh"));
        assert!(resolve("/bin/sh").is_some());
        assert!(resolve("/bin/definitely-not-here").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_on_path_is_not_a_program() {
        let dir = std::env::temp_dir().join(format!("herdr-updater-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("herdr-updater-not-executable");
        std::fs::write(&plain, "#!/bin/sh\n").unwrap();
        assert!(!executable(&plain));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_binary_is_a_spawn_error_not_a_panic() {
        let e = run("herdr-updater-no-such-binary", &[], Duration::from_secs(5)).unwrap_err();
        assert!(matches!(e, ExecError::Spawn(_)));
    }
}
