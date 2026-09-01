use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::exec;

const STATE_FILE: &str = "schedule-state.json";
const LOCK_DIR: &str = "schedule.lock";
const LOCK_OWNER_FILE: &str = "owner.json";
/// Last-resort reclaim age, used only when ownership cannot be *proved* dead.
///
/// It used to be the only recovery path, and that was the whole bug: a run
/// killed by a signal never reaches `Drop`, so the lock directory outlived it
/// and every subsequent run reported "another scheduled check is already
/// running" and exited 0 for two full hours. Observed on vspc-wsl 2026-09-01 —
/// a SIGTERM at 09:18:45, then ~25 consecutive no-op runs, then the first real
/// check at 11:20:40, exactly one staleness window later. A scheduler that is
/// installed, firing, and reporting success while doing nothing is the precise
/// failure this crate exists to prevent.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);
const MIN_SCHEDULER_TICK: Duration = Duration::from_secs(60);
const MAX_SCHEDULER_TICK: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ScheduleState {
    pub last_attempt_unix_seconds: Option<u64>,
    pub last_success_unix_seconds: Option<u64>,
    pub last_fleet_sync_unix_seconds: Option<u64>,
    pub next_check_unix_seconds: Option<u64>,
    pub consecutive_failures: u32,
    pub last_exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub platform: &'static str,
    pub installed: bool,
    pub resource: String,
    pub state: ScheduleState,
}

/// Read the scheduler state without touching it, for callers that report on
/// the schedule rather than manage it.
pub fn describe(config_dir: &Path) -> Result<Status, String> {
    status(config_dir)
}

/// What the lock looks like right now, for reporting rather than acquiring.
#[derive(Debug, Clone)]
pub struct LockReport {
    pub verdict: LockVerdict,
    pub pid: Option<u32>,
    pub held_for: Option<Duration>,
}

/// Describe the lock without touching it. `None` when no run holds one.
pub fn describe_lock(config_dir: &Path) -> Option<LockReport> {
    let lock = config_dir.join(LOCK_DIR);
    if !fs::symlink_metadata(&lock).is_ok_and(|m| m.file_type().is_dir()) {
        return None;
    }
    let owner = read_owner(&lock);
    let held_for = fs::symlink_metadata(&lock)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    Some(LockReport {
        verdict: inspect_owner(owner.as_ref()),
        pid: owner.as_ref().map(|owner| owner.pid),
        held_for,
    })
}

pub struct RunLease {
    lock: PathBuf,
    state_path: PathBuf,
    state: ScheduleState,
}

struct StateBackup {
    path: PathBuf,
    original: Option<Vec<u8>>,
    changed: bool,
}

impl StateBackup {
    fn restore(self) -> Result<(), String> {
        if !self.changed {
            return Ok(());
        }
        match self.original {
            Some(bytes) => write_owned(&self.path, &bytes),
            None => remove_owned(&self.path),
        }
    }
}

impl Drop for RunLease {
    fn drop(&mut self) {
        // The owner file lives inside the directory, so the directory is no
        // longer empty and `remove_dir` alone would leave the lock behind.
        let _ = fs::remove_file(self.lock.join(LOCK_OWNER_FILE));
        let _ = fs::remove_dir(&self.lock);
    }
}

/// Who holds the lock, written inside it so a later run can ask whether that
/// process still exists instead of only asking how old the directory is.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockOwner {
    pub pid: u32,
    /// Distinguishes boots, so a PID reused after a reboot is not mistaken for
    /// the original holder. `None` where the platform does not expose one.
    #[serde(default)]
    pub boot: Option<String>,
    pub acquired_unix_seconds: u64,
}

/// What we could establish about the current holder of the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockVerdict {
    /// A live process holds it; a concurrent run is genuinely in progress.
    Held,
    /// The recorded owner is provably gone. Reclaim immediately.
    OwnerGone,
    /// Ownership is unknowable here, so only age can decide.
    Unknown,
}

fn boot_id() -> Option<String> {
    // Linux exposes one directly. Elsewhere we simply have no boot identity and
    // fall back to PID liveness alone, which is still far better than waiting
    // out a two-hour timeout.
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Is `pid` still running?
///
/// `Some(false)` is the only answer that authorises reclaiming a lock, so every
/// uncertain case must return `None` and let the age check decide. Guessing
/// "dead" wrongly would let two runs mutate plugins at once.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false); // never a valid holder
    }
    // /proc is authoritative and needs no subprocess.
    Some(Path::new(&format!("/proc/{pid}")).exists())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }
    // `kill -0` reports liveness without signalling. Exit 0 means alive; a
    // non-zero exit means gone — "not ours" cannot happen for a lock this
    // same user wrote.
    let pid = pid.to_string();
    match exec::run("kill", &["-0", &pid], Duration::from_secs(5)) {
        Ok(output) => Some(output.ok()),
        Err(_) => None,
    }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> Option<bool> {
    None
}

fn read_owner(lock: &Path) -> Option<LockOwner> {
    let bytes = read_optional_state_bytes(&lock.join(LOCK_OWNER_FILE)).ok()??;
    serde_json::from_slice(&bytes).ok()
}

/// Decide whether the holder is still alive, from the owner file alone.
pub(crate) fn inspect_owner(owner: Option<&LockOwner>) -> LockVerdict {
    // No owner file: written by a build that predates ownership, so all we can
    // do is fall back to age.
    let Some(owner) = owner else {
        return LockVerdict::Unknown;
    };
    // A different boot means the holder cannot exist, whatever its PID says
    // today — and PIDs are recycled aggressively after a reboot.
    if let (Some(recorded), Some(current)) = (owner.boot.as_deref(), boot_id()) {
        if recorded != current {
            return LockVerdict::OwnerGone;
        }
    }
    match process_alive(owner.pid) {
        Some(true) => LockVerdict::Held,
        Some(false) => LockVerdict::OwnerGone,
        None => LockVerdict::Unknown,
    }
}

fn write_owner(lock: &Path) -> Result<(), String> {
    let owner = LockOwner {
        pid: std::process::id(),
        boot: boot_id(),
        acquired_unix_seconds: now(),
    };
    let bytes =
        serde_json::to_vec(&owner).map_err(|error| format!("cannot encode lock owner: {error}"))?;
    write_owned(&lock.join(LOCK_OWNER_FILE), &bytes)
}

impl RunLease {
    pub fn fleet_sync_due(&self, config: &Config) -> bool {
        config.scheduled_fleet_sync
            && elapsed_since(self.state.last_fleet_sync_unix_seconds)
                >= config.fleet_sync_every().as_secs()
    }

    pub fn finish(
        &mut self,
        config: &Config,
        exit_code: i32,
        successful: bool,
        fleet_synced: bool,
    ) -> Result<(), String> {
        let now = now();
        self.state.last_attempt_unix_seconds = Some(now);
        self.state.last_exit_code = Some(exit_code);
        if successful {
            self.state.last_success_unix_seconds = Some(now);
            self.state.consecutive_failures = 0;
        } else {
            self.state.consecutive_failures = self.state.consecutive_failures.saturating_add(1);
        }
        if fleet_synced {
            self.state.last_fleet_sync_unix_seconds = Some(now);
        }
        let delay = if successful {
            config.check_every().as_secs()
        } else {
            retry_delay(self.state.consecutive_failures, config.check_every()).as_secs()
        };
        self.state.next_check_unix_seconds = Some(
            now.saturating_add(delay)
                .saturating_add(deterministic_jitter(now, config.jitter_value())),
        );
        persist_state(&self.state_path, &self.state)
    }

    pub fn defer_for_quiet_hours(&mut self, config: &Config) -> Result<(), String> {
        self.state.next_check_unix_seconds = Some(
            now()
                .saturating_add(5 * 60)
                .saturating_add(deterministic_jitter(now(), config.jitter_value())),
        );
        persist_state(&self.state_path, &self.state)
    }
}

pub fn begin(config_dir: &Path) -> Result<Option<RunLease>, String> {
    fs::create_dir_all(config_dir)
        .map_err(|error| format!("cannot create {}: {error}", config_dir.display()))?;
    let lock = config_dir.join(LOCK_DIR);
    match fs::create_dir(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&lock)
                .map_err(|error| format!("cannot inspect {}: {error}", lock.display()))?;
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "{} is not the updater's lock directory",
                    lock.display()
                ));
            }
            // Ask who holds it before asking how old it is. A run killed by a
            // signal leaves this directory behind with no chance to clean up,
            // and age alone cannot tell that apart from a run still working.
            let reclaim = match inspect_owner(read_owner(&lock).as_ref()) {
                LockVerdict::Held => return Ok(None),
                LockVerdict::OwnerGone => true,
                LockVerdict::Unknown => metadata
                    .modified()
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age >= LOCK_STALE_AFTER),
            };
            if !reclaim {
                return Ok(None);
            }
            let _ = fs::remove_file(lock.join(LOCK_OWNER_FILE));
            fs::remove_dir(&lock)
                .map_err(|error| format!("cannot clear stale {}: {error}", lock.display()))?;
            fs::create_dir(&lock)
                .map_err(|error| format!("cannot acquire {}: {error}", lock.display()))?;
        }
        Err(error) => return Err(format!("cannot acquire {}: {error}", lock.display())),
    }
    // Best-effort: a lock without an owner file still works, it just falls back
    // to the age check, so a write failure here must not abort the run.
    let _ = write_owner(&lock);
    let state_path = config_dir.join(STATE_FILE);
    let state = read_state(&state_path)?;
    Ok(Some(RunLease {
        lock,
        state_path,
        state,
    }))
}

pub fn check_due(config_dir: &Path, config: &Config) -> Result<bool, String> {
    let state = read_state(&config_dir.join(STATE_FILE))?;
    let fallback_due =
        elapsed_since(state.last_attempt_unix_seconds) >= config.check_every().as_secs();
    Ok(state
        .next_check_unix_seconds
        .map_or(fallback_due, |next| now() >= next))
}

pub fn quiet_now(config: &Config, timeout: Duration) -> Result<bool, String> {
    let Some(raw) = config.quiet_hours.as_deref() else {
        return Ok(false);
    };
    let (start, end) = config::parse_quiet_hours(raw)?;
    let current = local_minutes(timeout)?;
    Ok(in_window(current, start, end))
}

pub fn cmd_schedule(
    mode: &str,
    config_dir: &Path,
    config_path: &Path,
    config: &Config,
    json: bool,
    timeout: Duration,
) -> i32 {
    let result = match mode {
        "install" => install_with_initial_state(config_dir, config_path, config, timeout),
        "remove" => remove(config_dir, timeout),
        "status" => status(config_dir),
        _ => Err("schedule mode must be run, install, status, or remove".into()),
    };
    match result {
        Ok(status) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".into())
                );
            } else {
                println!(
                    "schedule: {} on {}\nresource: {}",
                    if status.installed {
                        "installed"
                    } else {
                        "not installed"
                    },
                    status.platform,
                    status.resource
                );
                if let Some(next) = status.state.next_check_unix_seconds {
                    println!("next check: {}", crate::clock::describe_unix(next));
                }
                if let Some(last) = status.state.last_success_unix_seconds {
                    println!("last success: {}", crate::clock::describe_unix(last));
                }
                if status.state.consecutive_failures > 0 {
                    println!(
                        "consecutive failures: {}",
                        status.state.consecutive_failures
                    );
                }
            }
            0
        }
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            if mode == "status" {
                2
            } else {
                1
            }
        }
    }
}

fn install_with_initial_state(
    config_dir: &Path,
    config_path: &Path,
    config: &Config,
    timeout: Duration,
) -> Result<Status, String> {
    let backup = prepare_initial_state(config_dir, config)?;
    match install(config_dir, config_path, config, timeout) {
        Ok(status) => Ok(status),
        Err(error) => match backup.restore() {
            Ok(()) => Err(error),
            Err(restore_error) => Err(format!(
                "{error}; additionally could not restore schedule state: {restore_error}"
            )),
        },
    }
}

#[cfg(target_os = "linux")]
fn install(
    config_dir: &Path,
    config_path: &Path,
    config: &Config,
    timeout: Duration,
) -> Result<Status, String> {
    if !exec::have("systemctl") {
        return Err("systemctl is required to install the Linux user timer".into());
    }
    let home = home()?;
    let unit_dir = home.join(".config/systemd/user");
    let service = unit_dir.join("herdr-updater.service");
    let timer = unit_dir.join("herdr-updater.timer");
    let executable = current_exe()?;
    let tick = scheduler_tick(config);
    let service_text = format!(
        "[Unit]\nDescription=Check Herdr core and plugins\n\n[Service]\nType=oneshot\nSuccessExitStatus=1\nEnvironment=PATH={}\nExecStart={} schedule run --config {}\n",
        scheduler_path(),
        systemd_quote(&executable),
        systemd_quote(config_path)
    );
    let timer_text = format!(
        "[Unit]\nDescription=Schedule Herdr update checks\n\n[Timer]\nOnBootSec={}\nOnUnitActiveSec={}\nPersistent=true\nUnit=herdr-updater.service\n\n[Install]\nWantedBy=timers.target\n",
        if config.initial_delay_value().is_zero() {
            1
        } else {
            tick.as_secs()
        },
        tick.as_secs(),
    );
    write_owned(&service, service_text.as_bytes())?;
    write_owned(&timer, timer_text.as_bytes())?;
    run_ok(
        "systemctl",
        &["--user", "daemon-reload"],
        timeout,
        "systemd reload",
    )?;
    run_ok(
        "systemctl",
        &["--user", "enable", "--now", "herdr-updater.timer"],
        timeout,
        "systemd timer enable",
    )?;
    status(config_dir)
}

#[cfg(target_os = "linux")]
fn remove(config_dir: &Path, timeout: Duration) -> Result<Status, String> {
    if exec::have("systemctl") {
        let _ = exec::run(
            "systemctl",
            &["--user", "disable", "--now", "herdr-updater.timer"],
            timeout,
        );
    }
    let unit_dir = home()?.join(".config/systemd/user");
    remove_owned(&unit_dir.join("herdr-updater.timer"))?;
    remove_owned(&unit_dir.join("herdr-updater.service"))?;
    if exec::have("systemctl") {
        run_ok(
            "systemctl",
            &["--user", "daemon-reload"],
            timeout,
            "systemd reload",
        )?;
    }
    status(config_dir)
}

#[cfg(target_os = "linux")]
fn status(config_dir: &Path) -> Result<Status, String> {
    let resource = home()?.join(".config/systemd/user/herdr-updater.timer");
    Ok(Status {
        platform: "linux/systemd-user",
        installed: is_regular(&resource),
        resource: resource.display().to_string(),
        state: read_state(&config_dir.join(STATE_FILE))?,
    })
}

#[cfg(target_os = "macos")]
fn install(
    config_dir: &Path,
    config_path: &Path,
    config: &Config,
    timeout: Duration,
) -> Result<Status, String> {
    if !exec::have("launchctl") {
        return Err("launchctl is required to install the macOS user agent".into());
    }
    let label = "io.github.diegopzz.herdr-updater";
    let resource = home()?.join(format!("Library/LaunchAgents/{label}.plist"));
    let executable = current_exe()?;
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>schedule</string><string>run</string><string>--config</string><string>{}</string></array>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>\n<key>StartInterval</key><integer>{}</integer>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
        xml_escape(&executable.display().to_string()),
        xml_escape(&config_path.display().to_string()),
        xml_escape(&scheduler_path()),
        scheduler_tick(config).as_secs()
    );
    write_owned(&resource, plist.as_bytes())?;
    let domain = launch_domain(timeout)?;
    launchd_unload(&domain, label, timeout)?;
    run_ok(
        "launchctl",
        &["bootstrap", &domain, resource.to_string_lossy().as_ref()],
        timeout,
        "launchd bootstrap",
    )?;
    status(config_dir)
}

#[cfg(target_os = "macos")]
fn remove(config_dir: &Path, timeout: Duration) -> Result<Status, String> {
    let label = "io.github.diegopzz.herdr-updater";
    let resource = home()?.join(format!("Library/LaunchAgents/{label}.plist"));
    if exec::have("launchctl") {
        let domain = launch_domain(timeout)?;
        launchd_unload(&domain, label, timeout)?;
    }
    remove_owned(&resource)?;
    status(config_dir)
}

#[cfg(target_os = "macos")]
fn status(config_dir: &Path) -> Result<Status, String> {
    let resource = home()?.join("Library/LaunchAgents/io.github.diegopzz.herdr-updater.plist");
    Ok(Status {
        platform: "macos/launchd-user",
        installed: is_regular(&resource),
        resource: resource.display().to_string(),
        state: read_state(&config_dir.join(STATE_FILE))?,
    })
}

#[cfg(target_os = "macos")]
fn launch_domain(timeout: Duration) -> Result<String, String> {
    let output =
        exec::run("id", &["-u"], timeout).map_err(|error| format!("cannot read uid: {error}"))?;
    if !output.ok() || !output.trimmed().bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("cannot read the current macOS uid".into());
    }
    Ok(format!("gui/{}", output.trimmed()))
}

#[cfg(any(target_os = "macos", test))]
fn launchd_service_target(domain: &str, label: &str) -> String {
    format!("{domain}/{label}")
}

#[cfg(target_os = "macos")]
fn launchd_unload(domain: &str, label: &str, timeout: Duration) -> Result<(), String> {
    let service_target = launchd_service_target(domain, label);
    let before = exec::run("launchctl", &["print", &service_target], timeout)
        .map_err(|error| format!("launchd status: {error}"))?;
    if !before.ok() {
        return if launchd_service_not_loaded(&before) {
            Ok(())
        } else {
            Err(format!(
                "launchd status exited {}: {}",
                before.code,
                before.stderr.lines().next().unwrap_or("no stderr")
            ))
        };
    }
    run_ok(
        "launchctl",
        &["bootout", &service_target],
        timeout,
        "launchd bootout",
    )?;
    let after = exec::run("launchctl", &["print", &service_target], timeout)
        .map_err(|error| format!("launchd unload verification: {error}"))?;
    if launchd_service_not_loaded(&after) {
        Ok(())
    } else if after.ok() {
        Err(format!("launchd service {service_target} is still loaded"))
    } else {
        Err(format!(
            "launchd unload verification exited {}: {}",
            after.code,
            after.stderr.lines().next().unwrap_or("no stderr")
        ))
    }
}

#[cfg(any(target_os = "macos", test))]
fn launchd_service_not_loaded(output: &exec::Output) -> bool {
    if output.ok() {
        return false;
    }
    let message = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    message.contains("could not find service") || message.contains("service not found")
}

#[cfg(target_os = "windows")]
fn install(
    config_dir: &Path,
    config_path: &Path,
    config: &Config,
    timeout: Duration,
) -> Result<Status, String> {
    if !exec::have("schtasks") {
        return Err("schtasks is required to install the Windows user task".into());
    }
    let wrapper = config_dir.join("schedule-run.ps1");
    let executable = current_exe()?;
    let script = format!(
        "$ErrorActionPreference = 'Stop'\r\n$env:PATH = '{}'\r\n& '{}' schedule run --config '{}'\r\n$code = $LASTEXITCODE\r\nif ($code -eq 1) {{ exit 0 }}\r\nexit $code\r\n",
        ps_quote(&scheduler_path()),
        ps_quote(&executable.display().to_string()),
        ps_quote(&config_path.display().to_string())
    );
    write_owned(&wrapper, script.as_bytes())?;
    let task_command = format!(
        "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
        wrapper.display()
    );
    let (schedule, modifier) = windows_frequency(scheduler_tick(config));
    run_ok(
        "schtasks",
        &[
            "/Create",
            "/TN",
            "Herdr Updater",
            "/TR",
            &task_command,
            "/SC",
            schedule,
            "/MO",
            &modifier,
            "/RL",
            "LIMITED",
            "/F",
        ],
        timeout,
        "Windows task creation",
    )?;
    if config.initial_delay_value().is_zero() {
        run_ok(
            "schtasks",
            &["/Run", "/TN", "Herdr Updater"],
            timeout,
            "Windows task start",
        )?;
    }
    status(config_dir)
}

#[cfg(target_os = "windows")]
fn remove(config_dir: &Path, timeout: Duration) -> Result<Status, String> {
    if exec::have("schtasks") {
        let _ = exec::run(
            "schtasks",
            &["/Delete", "/TN", "Herdr Updater", "/F"],
            timeout,
        );
    }
    remove_owned(&config_dir.join("schedule-run.ps1"))?;
    status(config_dir)
}

#[cfg(target_os = "windows")]
fn status(config_dir: &Path) -> Result<Status, String> {
    let resource = config_dir.join("schedule-run.ps1");
    let installed = is_regular(&resource)
        && exec::run(
            "schtasks",
            &["/Query", "/TN", "Herdr Updater"],
            Duration::from_secs(10),
        )
        .is_ok_and(|output| output.ok());
    Ok(Status {
        platform: "windows/task-scheduler-user",
        installed,
        resource: resource.display().to_string(),
        state: read_state(&config_dir.join(STATE_FILE))?,
    })
}

#[cfg(target_os = "windows")]
fn windows_frequency(interval: Duration) -> (&'static str, String) {
    if interval.as_secs() >= 24 * 60 * 60 {
        (
            "DAILY",
            interval
                .as_secs()
                .div_ceil(24 * 60 * 60)
                .clamp(1, 365)
                .to_string(),
        )
    } else {
        (
            "MINUTE",
            interval.as_secs().div_ceil(60).clamp(1, 1439).to_string(),
        )
    }
}

fn scheduler_tick(config: &Config) -> Duration {
    let mut tick = config.check_every().min(MAX_SCHEDULER_TICK);
    for candidate in [config.initial_delay_value(), config.jitter_value()] {
        if !candidate.is_zero() {
            tick = tick.min(candidate);
        }
    }
    Duration::from_secs(tick.as_secs().max(MIN_SCHEDULER_TICK.as_secs()))
}

fn prepare_initial_state(config_dir: &Path, config: &Config) -> Result<StateBackup, String> {
    let path = config_dir.join(STATE_FILE);
    let original = read_optional_state_bytes(&path)?;
    let mut state = match original.as_deref() {
        Some(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| format!("cannot parse {}: {error}", path.display()))?,
        None => ScheduleState::default(),
    };
    let changed = state.next_check_unix_seconds.is_none()
        && state.last_attempt_unix_seconds.is_none()
        && state.last_success_unix_seconds.is_none();
    if changed {
        let current = now();
        state.next_check_unix_seconds = Some(
            current
                .saturating_add(config.initial_delay_value().as_secs())
                .saturating_add(deterministic_jitter(current, config.jitter_value())),
        );
        persist_state(&path, &state)?;
    }
    Ok(StateBackup {
        path,
        original,
        changed,
    })
}

fn read_optional_state_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 {
        return Err(format!("{} is not a bounded regular file", path.display()));
    }
    fs::read(path)
        .map(Some)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn read_state(path: &Path) -> Result<ScheduleState, String> {
    let Some(bytes) = read_optional_state_bytes(path)? else {
        return Ok(ScheduleState::default());
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

fn persist_state(path: &Path, state: &ScheduleState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("cannot encode schedule state: {error}"))?;
    write_owned(path, &bytes)
}

fn write_owned(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(format!(
                "refusing to replace non-regular {}",
                path.display()
            ));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn remove_owned(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if !metadata.file_type().is_file() {
        return Err(format!("refusing to remove non-regular {}", path.display()));
    }
    fs::remove_file(path).map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

fn is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn run_ok(program: &str, args: &[&str], timeout: Duration, label: &str) -> Result<(), String> {
    let output = exec::run(program, args, timeout).map_err(|error| format!("{label}: {error}"))?;
    if output.ok() {
        Ok(())
    } else {
        Err(format!(
            "{label} exited {}: {}",
            output.code,
            output.stderr.lines().next().unwrap_or("no stderr")
        ))
    }
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("cannot resolve updater executable: {error}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot resolve the user home directory".into())
}

/// The PATH to bake into the units we generate.
///
/// `schedule install` runs from a shell where `herdr` resolves -- that is how
/// the user got here. The scheduled run does not: systemd hands a user unit a
/// bare `/usr/local/sbin:...:/bin`, launchd is stricter still, and neither
/// includes `~/.local/bin`, where the installer puts `herdr`. Every check then
/// failed with `herdr status: could not run: No such file or directory`, while
/// running the same command by hand still worked -- so the breakage was
/// invisible exactly where anyone would look for it.
///
/// Carrying the installing shell's PATH forward puts the scheduled run in the
/// same environment as the manual one. The directory holding our own
/// executable leads, because `herdr` sitting beside `herdr-updater` is the
/// layout the installer produces and the one entry we can be sure about.
fn scheduler_path() -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Ok(exe) = current_exe() {
        if let Some(dir) = exe.parent() {
            let dir = dir.display().to_string();
            if !dir.is_empty() {
                parts.push(dir);
            }
        }
    }
    let inherited = std::env::var("PATH").unwrap_or_default();
    let source = if inherited.trim().is_empty() {
        default_path()
    } else {
        &inherited
    };
    for entry in source.split(PATH_SEP) {
        if !entry.is_empty() && !parts.iter().any(|p| p == entry) {
            parts.push(entry.to_string());
        }
    }
    parts.join(PATH_SEP_STR)
}

/// PATH entry separator for the platform whose scheduler we are writing for.
#[cfg(windows)]
const PATH_SEP: char = ';';
#[cfg(not(windows))]
const PATH_SEP: char = ':';
#[cfg(windows)]
const PATH_SEP_STR: &str = ";";
#[cfg(not(windows))]
const PATH_SEP_STR: &str = ":";

/// Last resort when the installing process itself has no PATH -- rare, but a
/// unit with an empty PATH is strictly worse than one with a conventional guess.
#[cfg(windows)]
fn default_path() -> &'static str {
    r"C:\Windows\system32;C:\Windows"
}
#[cfg(not(windows))]
fn default_path() -> &'static str {
    "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
}

#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "windows")]
fn ps_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn local_minutes(timeout: Duration) -> Result<u16, String> {
    #[cfg(target_os = "windows")]
    let output = exec::run(
        "powershell",
        &["-NoProfile", "-Command", "(Get-Date).ToString('HH:mm')"],
        timeout,
    );
    #[cfg(not(target_os = "windows"))]
    let output = exec::run("date", &["+%H:%M"], timeout);
    let output = output.map_err(|error| format!("cannot read local time: {error}"))?;
    let (hour, minute) = output
        .trimmed()
        .split_once(':')
        .ok_or_else(|| "local time command returned an invalid value".to_string())?;
    let hour: u16 = hour
        .parse()
        .map_err(|_| "local time command returned an invalid hour".to_string())?;
    let minute: u16 = minute
        .parse()
        .map_err(|_| "local time command returned an invalid minute".to_string())?;
    if hour >= 24 || minute >= 60 {
        return Err("local time command returned an out-of-range value".into());
    }
    Ok(hour * 60 + minute)
}

fn in_window(current: u16, start: u16, end: u16) -> bool {
    if start < end {
        current >= start && current < end
    } else {
        current >= start || current < end
    }
}

fn retry_delay(failures: u32, normal: Duration) -> Duration {
    let exponent = failures.saturating_sub(1).min(8);
    let seconds = 5 * 60u64.saturating_mul(1u64 << exponent);
    Duration::from_secs(seconds.min(normal.as_secs().max(5 * 60)))
}

fn deterministic_jitter(seed: u64, maximum: Duration) -> u64 {
    let max = maximum.as_secs();
    if max == 0 {
        0
    } else {
        seed.rotate_left(17).wrapping_mul(0x9e3779b97f4a7c15) % (max + 1)
    }
}

fn elapsed_since(value: Option<u64>) -> u64 {
    value.map_or(u64::MAX, |value| now().saturating_sub(value))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-updater-lock-{tag}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_reclaimed_without_waiting() {
        // The regression this exists for: a run killed by a signal never
        // reaches Drop, and every later run used to report "already running"
        // and exit 0 for two hours. PID 0 can never be a live holder.
        let dir = scratch("dead");
        let lock = dir.join(LOCK_DIR);
        fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pid: 0,
            boot: boot_id(),
            acquired_unix_seconds: now(),
        };
        fs::write(
            lock.join(LOCK_OWNER_FILE),
            serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();

        let lease = begin(&dir).expect("begin must not error").expect(
            "a lock owned by a dead process must be reclaimed immediately, not after 2 hours",
        );
        drop(lease);
        // Drop must leave nothing behind, owner file included.
        assert!(!lock.exists(), "lock survived the lease");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_lock_held_by_a_live_process_is_respected() {
        let dir = scratch("live");
        let lock = dir.join(LOCK_DIR);
        fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pid: std::process::id(), // this test is alive by definition
            boot: boot_id(),
            acquired_unix_seconds: now(),
        };
        fs::write(
            lock.join(LOCK_OWNER_FILE),
            serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();
        assert!(
            begin(&dir).unwrap().is_none(),
            "a genuinely concurrent run must still be refused"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_lock_from_a_previous_boot_is_gone_whatever_its_pid_says() {
        // PIDs are recycled aggressively across a reboot, so a live PID today
        // says nothing about a holder recorded before the machine restarted.
        let owner = LockOwner {
            pid: std::process::id(),
            boot: Some("00000000-0000-0000-0000-000000000000".into()),
            acquired_unix_seconds: now(),
        };
        if boot_id().is_some() {
            assert_eq!(inspect_owner(Some(&owner)), LockVerdict::OwnerGone);
        }
    }

    #[test]
    fn a_lock_with_no_owner_file_falls_back_to_age() {
        // Written by a build that predates ownership: unknowable, so the age
        // check must remain the decider rather than a guess either way.
        assert_eq!(inspect_owner(None), LockVerdict::Unknown);
    }

    #[test]
    fn acquiring_records_this_process_as_the_owner() {
        let dir = scratch("owner");
        let lease = begin(&dir).unwrap().expect("free lock must be acquirable");
        let recorded = read_owner(&dir.join(LOCK_DIR)).expect("owner file must be written");
        assert_eq!(recorded.pid, std::process::id());
        assert_eq!(recorded.boot, boot_id());
        drop(lease);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn quiet_hours_support_windows_that_cross_midnight() {
        assert!(in_window(23 * 60, 22 * 60, 6 * 60));
        assert!(in_window(5 * 60, 22 * 60, 6 * 60));
        assert!(!in_window(12 * 60, 22 * 60, 6 * 60));
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        assert_eq!(
            retry_delay(1, Duration::from_secs(24 * 60 * 60)).as_secs(),
            300
        );
        assert_eq!(
            retry_delay(3, Duration::from_secs(24 * 60 * 60)).as_secs(),
            1200
        );
        assert_eq!(
            retry_delay(30, Duration::from_secs(60 * 60)).as_secs(),
            3600
        );
    }

    #[test]
    fn jitter_never_exceeds_the_configured_maximum() {
        for seed in 0..1_000 {
            assert!(deterministic_jitter(seed, Duration::from_secs(300)) <= 300);
        }
    }

    #[test]
    fn scheduler_tick_is_short_enough_to_observe_jittered_deadlines() {
        let config = Config::default();
        assert_eq!(scheduler_tick(&config), Duration::from_secs(5 * 60));

        let config = Config {
            initial_delay: "0s".into(),
            jitter: "90s".into(),
            ..Config::default()
        };
        assert_eq!(scheduler_tick(&config), Duration::from_secs(90));

        let config = Config {
            initial_delay: "1s".into(),
            jitter: "1s".into(),
            ..Config::default()
        };
        assert_eq!(scheduler_tick(&config), Duration::from_secs(60));
    }

    #[test]
    fn scheduler_path_leads_with_our_own_directory() {
        // the installed layout is `herdr` beside `herdr-updater`, so the
        // directory we run from is the one entry we can be sure about
        let path = scheduler_path();
        let exe = current_exe().expect("current exe");
        let dir = exe.parent().expect("exe dir").display().to_string();
        assert!(path.starts_with(&dir), "{path} should start with {dir}");
    }

    #[test]
    fn scheduler_path_keeps_inherited_entries_without_duplicating_them() {
        let path = scheduler_path();
        for entry in std::env::var("PATH").unwrap_or_default().split(PATH_SEP) {
            if entry.is_empty() {
                continue;
            }
            assert!(
                path.split(PATH_SEP).any(|p| p == entry),
                "{entry} missing from {path}"
            );
        }
        let mut seen: Vec<&str> = path.split(PATH_SEP).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate entries in {path}");
    }

    #[test]
    fn scheduler_path_is_never_empty() {
        // a service manager may hand us no PATH at all; the unit still needs one
        assert!(!scheduler_path().is_empty());
    }

    #[test]
    fn launchd_bootout_uses_a_single_domain_qualified_service_target() {
        assert_eq!(
            launchd_service_target("gui/501", "io.github.diegopzz.herdr-updater"),
            "gui/501/io.github.diegopzz.herdr-updater"
        );
    }

    #[test]
    fn launchd_only_ignores_a_confirmed_missing_service() {
        let missing = exec::Output {
            code: 113,
            stdout: String::new(),
            stderr: "Bad request. Could not find service in domain for user gui: 501".into(),
        };
        assert!(launchd_service_not_loaded(&missing));

        let denied = exec::Output {
            code: 1,
            stdout: String::new(),
            stderr: "Operation not permitted".into(),
        };
        assert!(!launchd_service_not_loaded(&denied));
    }

    #[test]
    fn installing_a_schedule_seeds_and_can_restore_initial_state() {
        let unique = format!("herdr-updater-schedule-{}-{}", std::process::id(), now());
        let root = std::env::temp_dir().join(unique);
        let config = Config {
            initial_delay: "5m".into(),
            jitter: "0s".into(),
            ..Config::default()
        };
        let before = now();
        let backup = prepare_initial_state(&root, &config).unwrap();
        let state = read_state(&root.join(STATE_FILE)).unwrap();
        assert!(state
            .next_check_unix_seconds
            .is_some_and(|next| { next >= before + 300 && next <= now().saturating_add(300) }));
        backup.restore().unwrap();
        assert!(!root.join(STATE_FILE).exists());
        let _ = fs::remove_dir(root);
    }
}
