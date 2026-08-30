use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::exec;

const STATE_FILE: &str = "schedule-state.json";
const LOCK_DIR: &str = "schedule.lock";
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
struct Status {
    platform: &'static str,
    installed: bool,
    resource: String,
    state: ScheduleState,
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
        let _ = fs::remove_dir(&self.lock);
    }
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
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age >= LOCK_STALE_AFTER);
            if !stale {
                return Ok(None);
            }
            fs::remove_dir(&lock)
                .map_err(|error| format!("cannot clear stale {}: {error}", lock.display()))?;
            fs::create_dir(&lock)
                .map_err(|error| format!("cannot acquire {}: {error}", lock.display()))?;
        }
        Err(error) => return Err(format!("cannot acquire {}: {error}", lock.display())),
    }
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
                    println!("next check: unix {next}");
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
        "[Unit]\nDescription=Check Herdr core and plugins\n\n[Service]\nType=oneshot\nSuccessExitStatus=1\nExecStart={} schedule run --config {}\n",
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
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>schedule</string><string>run</string><string>--config</string><string>{}</string></array>\n<key>StartInterval</key><integer>{}</integer>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
        xml_escape(&executable.display().to_string()),
        xml_escape(&config_path.display().to_string()),
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
        "$ErrorActionPreference = 'Stop'\r\n& '{}' schedule run --config '{}'\r\n$code = $LASTEXITCODE\r\nif ($code -eq 1) {{ exit 0 }}\r\nexit $code\r\n",
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
