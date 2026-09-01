//! `doctor` — one command that answers "why is this tool not doing anything?"
//!
//! Every other command in this crate reports on *Herdr*. This one reports on
//! the updater itself, because its characteristic failure is silence: a
//! schedule that was never installed, a `curl` that is not on PATH, a config
//! whose directory is not writable, an API budget that ran out three plugins
//! ago. Each of those produces a tool that runs, exits, and changes nothing —
//! and the exit code alone never says which one fired.
//!
//! Three levels, and the difference matters:
//!
//! * `ok` — checked, and working.
//! * `warn` — a capability is degraded or switched off. Something you asked for
//!   will not happen, but nothing is broken.
//! * `fail` — a check this tool depends on cannot run at all, so its answers
//!   about that area are not answers.
//!
//! `warn` exits 1 and `fail` exits 2, matching every other command: 1 needs
//! attention, 2 means a check is unknown.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::clock;
use crate::config::{self, Config, Policy};
use crate::exec;
use crate::fleet;
use crate::herdr;
use crate::history;
use crate::plugins;
use crate::schedule;
use crate::sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub area: &'static str,
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    /// What to do about it. Present on everything that is not `ok`, because a
    /// diagnostic that does not say what to change is just a complaint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub ok: usize,
    pub warnings: usize,
    pub failures: usize,
}

fn ok(area: &'static str, name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        area,
        name,
        level: Level::Ok,
        detail: detail.into(),
        remedy: None,
    }
}

fn warn(
    area: &'static str,
    name: &'static str,
    detail: impl Into<String>,
    remedy: impl Into<String>,
) -> Check {
    Check {
        area,
        name,
        level: Level::Warn,
        detail: detail.into(),
        remedy: Some(remedy.into()),
    }
}

fn fail(
    area: &'static str,
    name: &'static str,
    detail: impl Into<String>,
    remedy: impl Into<String>,
) -> Check {
    Check {
        area,
        name,
        level: Level::Fail,
        detail: detail.into(),
        remedy: Some(remedy.into()),
    }
}

/// Run every check. `doctor` loads its own config rather than receiving one,
/// because a config that will not load is the single most likely reason
/// somebody is running this command, and aborting before the first check would
/// answer that question with the same silence being diagnosed.
pub fn run(override_path: Option<&Path>, herdr_bin: &str, json: bool, timeout: Duration) -> i32 {
    let mut checks = Vec::new();
    let loaded = config::load(override_path, herdr_bin, timeout);
    let config = match &loaded {
        Ok(loaded) => {
            checks.push(if loaded.existed {
                ok("config", "file", format!("{}", loaded.path.display()))
            } else {
                ok(
                    "config",
                    "file",
                    format!("{} (absent; built-in defaults)", loaded.path.display()),
                )
            });
            for warning in &loaded.warnings {
                checks.push(warn(
                    "config",
                    "unknown key",
                    warning.clone(),
                    "remove the key, or upgrade herdr-updater if it belongs to a newer build",
                ));
            }
            checks.push(match loaded.value.policy {
                Policy::Notify => ok(
                    "config",
                    "policy",
                    "notify — nothing is updated without an explicit apply",
                ),
                Policy::Auto => ok("config", "policy", "auto — safe updates are applied"),
            });
            loaded.value.clone()
        }
        Err(error) => {
            checks.push(fail(
                "config",
                "file",
                error.clone(),
                "fix the reported key, or pass --config to point at a different file",
            ));
            // Continue with defaults: the rest of the environment is still
            // worth reporting, and a broken config is usually not the only
            // thing wrong on a host somebody is already debugging.
            Config::default()
        }
    };

    checks.extend(herdr_checks(herdr_bin, &config, timeout));
    checks.extend(tool_checks());
    checks.extend(github_budget(timeout));

    let config_dir = loaded
        .as_ref()
        .ok()
        .and_then(|loaded| loaded.path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| config::resolve_dir(herdr_bin, timeout));
    checks.extend(state_checks(&config_dir, &config));
    checks.extend(fleet_checks(&config_dir));

    render(checks, json)
}

fn herdr_checks(herdr_bin: &str, config: &Config, timeout: Duration) -> Vec<Check> {
    let mut checks = Vec::new();
    match exec::resolve(herdr_bin) {
        Some(path) => checks.push(ok("herdr", "binary", path.display().to_string())),
        None => {
            checks.push(fail(
                "herdr",
                "binary",
                format!("{herdr_bin:?} is not on PATH"),
                "install Herdr, or set HERDR_BIN_PATH to its location",
            ));
            return checks;
        }
    }

    let state = herdr::inspect(herdr_bin, timeout);
    if let Some(error) = &state.error {
        checks.push(fail(
            "herdr",
            "status",
            error.clone(),
            "run `herdr status --json` directly and check that it is reachable",
        ));
        return checks;
    }
    checks.push(ok(
        "herdr",
        "installed",
        format!(
            "{} (protocol {}){}",
            state.installed,
            state.protocol,
            state
                .channel
                .as_deref()
                .map(|channel| format!(", channel {channel}"))
                .unwrap_or_default()
        ),
    ));
    checks.push(match state.latest.as_deref() {
        Some(latest) => ok(
            "herdr",
            "manifest",
            format!("latest {latest}, relation {:?}", state.relation),
        ),
        None => warn(
            "herdr",
            "manifest",
            "herdr.dev/latest.json could not be read",
            "core update checks report unknown until the network is reachable",
        ),
    });
    match herdr::gate(&state, config.allow_channel_mismatch) {
        Ok(()) => checks.push(ok("herdr", "core update", "eligible to update")),
        Err(herdr::Hold::UpToDate) => checks.push(ok("herdr", "core update", "already current")),
        Err(hold @ herdr::Hold::Unorderable) => checks.push(fail(
            "herdr",
            "core update",
            herdr::hold_text(&hold),
            "check that `herdr status --json` reports a version this build can parse",
        )),
        Err(hold @ herdr::Hold::NoLiveHandoff) => checks.push(warn(
            "herdr",
            "core update",
            herdr::hold_text(&hold),
            "stop the running server before updating core, or wait for a build with live handoff",
        )),
        Err(hold @ herdr::Hold::ChannelMismatch(_)) => checks.push(warn(
            "herdr",
            "core update",
            herdr::hold_text(&hold),
            "set allow_channel_mismatch = true only if latest.json describes your channel",
        )),
        Err(hold) => checks.push(warn(
            "herdr",
            "core update",
            herdr::hold_text(&hold),
            "no action needed; core updates are held while this is true",
        )),
    }

    match plugins::list_installed(herdr_bin, timeout) {
        Ok(installed) => checks.push(ok(
            "herdr",
            "plugins",
            format!("{} installed", installed.len()),
        )),
        Err(error) => checks.push(fail(
            "herdr",
            "plugins",
            error,
            "run `herdr plugin list --json` directly to see why it fails",
        )),
    }
    checks
}

/// Each external tool is reported with the capability it actually gates, so a
/// missing one reads as "this feature is off" rather than "something is
/// missing somewhere".
fn tool_checks() -> Vec<Check> {
    const REQUIRED: [(&str, &str, bool); 5] = [
        ("git", "resolving plugin refs against upstream", true),
        ("curl", "reading latest.json and the marketplace", true),
        (
            "gh",
            "authenticated GitHub compares with a 5000/hour budget",
            false,
        ),
        ("ssh", "fleet inventory and fleet sync", false),
        (
            "tar",
            "unpacking release binaries in the plugin launcher",
            false,
        ),
    ];
    REQUIRED
        .iter()
        .map(|(program, capability, required)| {
            if exec::have(program) {
                ok("tools", "present", format!("{program} — {capability}"))
            } else if *required {
                fail(
                    "tools",
                    "missing",
                    format!("{program} is not on PATH — {capability} cannot run"),
                    format!("install {program}"),
                )
            } else {
                warn(
                    "tools",
                    "missing",
                    format!("{program} is not on PATH — {capability} is unavailable"),
                    format!("install {program} to enable it"),
                )
            }
        })
        .collect()
}

fn gh_rate_limit(timeout: Duration) -> Option<String> {
    exec::have("gh")
        .then(|| exec::run("gh", &["api", "rate_limit"], timeout).ok())
        .flatten()
        .filter(exec::Output::ok)
        .map(|out| out.stdout)
}

fn curl_rate_limit(timeout: Duration) -> Option<String> {
    if !exec::have("curl") {
        return None;
    }
    let seconds = timeout.as_secs().max(1).to_string();
    exec::run(
        "curl",
        &[
            "-fsSL",
            "--max-time",
            &seconds,
            "-H",
            "Accept: application/vnd.github+json",
            "https://api.github.com/rate_limit",
        ],
        timeout,
    )
    .ok()
    .filter(exec::Output::ok)
    .map(|out| out.stdout)
}

/// How much GitHub API budget is left, because running out is the most common
/// reason a plugin check turns unknown and the least obvious to diagnose.
fn github_budget(timeout: Duration) -> Vec<Check> {
    // `gh` first for the authenticated budget, then curl — including when gh
    // is installed but unauthenticated, which fails here and would otherwise
    // report "budget could not be read" on a machine whose budget is readable.
    let response = gh_rate_limit(timeout).or_else(|| curl_rate_limit(timeout));
    let Some(response) = response else {
        return vec![warn(
            "github",
            "budget",
            "GitHub API budget could not be read",
            "plugin comparisons will report unknown while the API is unreachable",
        )];
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&response) else {
        return vec![warn(
            "github",
            "budget",
            "GitHub rate_limit response was not JSON",
            "no action needed unless plugin checks are also failing",
        )];
    };
    let core = &value["resources"]["core"];
    let remaining = core["remaining"].as_u64();
    let limit = core["limit"].as_u64();
    let reset = core["reset"].as_u64();
    match (remaining, limit) {
        (Some(remaining), Some(limit)) => {
            let resets = reset
                .map(|reset| format!(", resets {}", clock::relative_to_now(reset)))
                .unwrap_or_default();
            let detail = format!("{remaining}/{limit} requests remaining{resets}");
            // One inspection costs one request per GitHub-sourced plugin, so a
            // budget in single digits is about to turn a whole check unknown.
            if remaining < 10 {
                vec![warn(
                    "github",
                    "budget",
                    detail,
                    "run `gh auth login` for a 5000/hour authenticated budget",
                )]
            } else {
                vec![ok("github", "budget", detail)]
            }
        }
        _ => vec![warn(
            "github",
            "budget",
            "GitHub rate_limit response had no core budget",
            "no action needed unless plugin checks are also failing",
        )],
    }
}

/// The updater's own writable state: without it, history, rollback, and the
/// schedule lease all silently stop working.
fn state_checks(config_dir: &Path, config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    match writable(config_dir) {
        Ok(()) => checks.push(ok("state", "directory", config_dir.display().to_string())),
        Err(error) => {
            checks.push(fail(
                "state",
                "directory",
                format!("{}: {error}", config_dir.display()),
                "history, rollback, and scheduled runs need this directory to be writable",
            ));
            return checks;
        }
    }

    let history_path = history::path(config_dir);
    match history::read(&history_path) {
        Ok(events) => checks.push(ok(
            "state",
            "history",
            match events.last() {
                Some(last) => format!(
                    "{} events, last {}",
                    events.len(),
                    clock::describe_unix(last.unix_seconds)
                ),
                None => "no update history yet".to_string(),
            },
        )),
        Err(error) => checks.push(fail(
            "state",
            "history",
            error,
            "repair or remove the corrupt line; rollback reads this file",
        )),
    }

    match schedule::describe(config_dir) {
        Ok(status) => {
            checks.push(if status.installed {
                ok(
                    "schedule",
                    "installed",
                    format!("{} on {}", status.resource, status.platform),
                )
            } else {
                warn(
                    "schedule",
                    "installed",
                    format!("no background schedule on {}", status.platform),
                    "run `herdr-updater schedule install` for unattended checks",
                )
            });
            if let Some(next) = status.state.next_check_unix_seconds {
                // A due time far in the past is the signature of a scheduler
                // that is installed and not firing — a timer removed by hand, a
                // launchd agent never loaded, a machine that was asleep. The
                // native heartbeat is clamped to five minutes, so anything an
                // hour overdue is not late, it is not running.
                let overdue = clock::now().saturating_sub(next);
                checks.push(if overdue > 3_600 {
                    warn(
                        "schedule",
                        "next check",
                        format!(
                            "due {} and still pending; the scheduler does not appear to be firing",
                            clock::describe_unix(next)
                        ),
                        "reinstall with `herdr-updater schedule install`, then verify the resource above is active",
                    )
                } else {
                    ok("schedule", "next check", clock::describe_unix(next))
                });
            } else if config.startup_check {
                checks.push(ok(
                    "schedule",
                    "next check",
                    "not scheduled yet; the first startup check will set it",
                ));
            }
            if status.state.consecutive_failures > 0 {
                checks.push(warn(
                    "schedule",
                    "failures",
                    format!(
                        "{} consecutive failures, last exit code {}",
                        status.state.consecutive_failures,
                        status
                            .state
                            .last_exit_code
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    ),
                    "run `herdr-updater schedule run` by hand to see the failure",
                ));
            }
        }
        Err(error) => checks.push(warn(
            "schedule",
            "installed",
            error,
            "run `herdr-updater schedule status` for the full error",
        )),
    }
    checks
}

fn fleet_checks(config_dir: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let (hosts, source) = fleet::load_hosts();
    checks.push(match source {
        Some(path) => ok(
            "fleet",
            "hosts",
            format!("{} host(s) from {}", hosts.len(), path.display()),
        ),
        None => ok(
            "fleet",
            "hosts",
            "no hosts file; fleet and sync inspect this machine only",
        ),
    });
    let desired = sync::desired_path(config_dir);
    checks.push(if desired.is_file() {
        ok("fleet", "desired state", desired.display().to_string())
    } else {
        ok(
            "fleet",
            "desired state",
            format!("{} (absent; derived from this host)", desired.display()),
        )
    });
    checks
}

/// Prove the directory is writable by writing, rather than by reading
/// permission bits — the bits are advisory on network mounts and wrong under
/// containers and ACLs.
fn writable(directory: &Path) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let probe = directory.join(format!(".herdr-updater-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"probe").map_err(|error| error.to_string())?;
    let removed = std::fs::remove_file(&probe);
    removed.map_err(|error| error.to_string())
}

fn render(checks: Vec<Check>, json: bool) -> i32 {
    let failures = checks
        .iter()
        .filter(|check| check.level == Level::Fail)
        .count();
    let warnings = checks
        .iter()
        .filter(|check| check.level == Level::Warn)
        .count();
    let report = Report {
        ok: checks.len() - failures - warnings,
        warnings,
        failures,
        checks,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
        );
    } else {
        let mut area = "";
        for check in &report.checks {
            if check.area != area {
                area = check.area;
                println!("\n{area}");
            }
            println!(
                "  {:<5} {:<14} {}",
                match check.level {
                    Level::Ok => "OK",
                    Level::Warn => "WARN",
                    Level::Fail => "FAIL",
                },
                check.name,
                check.detail
            );
            if let Some(remedy) = &check.remedy {
                println!("        {:<14} -> {remedy}", "");
            }
        }
        println!(
            "\n{} ok, {} warning(s), {} failure(s)",
            report.ok, report.warnings, report.failures
        );
    }
    if report.failures > 0 {
        2
    } else if report.warnings > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_follows_the_worst_level_present() {
        assert_eq!(render(vec![ok("a", "b", "c")], true), 0);
        assert_eq!(render(vec![warn("a", "b", "c", "d")], true), 1);
        assert_eq!(
            render(
                vec![warn("a", "b", "c", "d"), fail("a", "b", "c", "d")],
                true
            ),
            2
        );
    }

    #[test]
    fn a_writable_directory_is_proven_by_writing_to_it() {
        let dir = std::env::temp_dir().join(format!("herdr-updater-doctor-{}", std::process::id()));
        assert!(writable(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
        // A path that cannot be a directory must not read as writable.
        let file = std::env::temp_dir().join(format!("herdr-doctor-file-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        assert!(writable(&file).is_err());
        let _ = std::fs::remove_file(&file);
    }
}
