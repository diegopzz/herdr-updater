use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{self, Config, Policy};
use crate::exec;
use crate::fleet::{self, FleetPlugin, HostState};
use crate::history::{self, Event, EventKind};
use crate::plugins;

const DESIRED_FILE: &str = "desired.toml";
const REMOTE_HERDR: &str = "if command -v herdr >/dev/null 2>&1; then H=herdr; elif [ -x \"$HOME/.local/bin/herdr\" ]; then H=\"$HOME/.local/bin/herdr\"; else echo NO_HERDR; exit 10; fi;";
const REMOTE_UPDATER: &str = "if command -v herdr-updater >/dev/null 2>&1; then U=herdr-updater; elif [ -x \"$HOME/.local/bin/herdr-updater\" ]; then U=\"$HOME/.local/bin/herdr-updater\"; else echo NO_UPDATER; exit 11; fi;";
const REMOTE_CONFIG_APPLY: &str = "if command -v herdr >/dev/null 2>&1; then H=herdr; elif [ -x \"$HOME/.local/bin/herdr\" ]; then H=\"$HOME/.local/bin/herdr\"; else exit 10; fi; D=$(\"$H\" plugin config-dir herdr-updater) || exit 12; umask 077; mkdir -p \"$D\" || exit 13; T=\"$D/config.toml.tmp.$$\"; trap 'rm -f \"$T\"' EXIT HUP INT TERM; cat >\"$T\" || exit 14; mv \"$T\" \"$D/config.toml\" || exit 15; trap - EXIT HUP INT TERM";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DesiredState {
    pub schema_version: u32,
    pub generated_unix_seconds: u64,
    pub settings: Config,
    pub plugins: Vec<DesiredPlugin>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DesiredPlugin {
    pub plugin_id: String,
    pub source: String,
    pub revision: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "action", content = "reason")]
pub enum SyncAction {
    Current,
    Install,
    Update,
    Enable,
    Disable,
    SyncSettings,
    Hold(String),
    Offline(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncDecision {
    pub host: String,
    pub target: String,
    pub plugin_id: String,
    pub source: Option<String>,
    pub previous: Option<String>,
    pub desired: Option<String>,
    pub action: SyncAction,
}

#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub desired_path: String,
    pub desired_exists: bool,
    pub generated_unix_seconds: u64,
    pub decisions: Vec<SyncDecision>,
    pub warnings: Vec<String>,
}

pub struct CommandRequest<'a> {
    pub mode: &'a str,
    pub hosts: &'a [String],
    pub json: bool,
    pub yes: bool,
}

pub fn desired_path(config_dir: &Path) -> PathBuf {
    config_dir.join(DESIRED_FILE)
}

pub fn export_desired(
    config_dir: &Path,
    config: &Config,
    herdr_bin: &str,
    timeout: Duration,
    json: bool,
) -> i32 {
    let (desired, warnings) = match derive_local(config, herdr_bin, timeout) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    let path = desired_path(config_dir);
    let text = match toml::to_string_pretty(&desired) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("herdr-updater: cannot encode desired state: {error}");
            return 2;
        }
    };
    if let Err(error) = write_regular(&path, text.as_bytes()) {
        eprintln!("herdr-updater: {error}");
        return 2;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path.display().to_string(),
                "plugins": desired.plugins.len(),
                "warnings": warnings,
            }))
            .unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!(
            "exported {} plugin(s) to {}",
            desired.plugins.len(),
            path.display()
        );
        for warning in warnings {
            println!("HOLD {warning}");
        }
    }
    0
}

fn derive_local(
    config: &Config,
    herdr_bin: &str,
    timeout: Duration,
) -> Result<(DesiredState, Vec<String>), String> {
    let installed = plugins::list_installed(herdr_bin, timeout)?;
    let mut desired = Vec::new();
    let mut warnings = Vec::new();
    for plugin in installed {
        let Some(source) = plugin.source else {
            warnings.push(format!("{} has no managed source", plugin.plugin_id));
            continue;
        };
        if source.kind == "local" {
            warnings.push(format!(
                "{} is linked locally and is intentionally absent from desired state",
                plugin.plugin_id
            ));
            continue;
        }
        if source.kind != "github" {
            warnings.push(format!("{} is not GitHub-managed", plugin.plugin_id));
            continue;
        }
        let (Some(owner), Some(repo), Some(revision)) =
            (source.owner, source.repo, source.resolved_commit)
        else {
            warnings.push(format!(
                "{} has incomplete GitHub metadata",
                plugin.plugin_id
            ));
            continue;
        };
        let mut install_source = format!("{owner}/{repo}");
        if let Some(subdir) = source.subdir {
            install_source.push('/');
            install_source.push_str(&subdir);
        }
        validate_desired_plugin(&DesiredPlugin {
            plugin_id: plugin.plugin_id.clone(),
            source: install_source.clone(),
            revision: revision.clone(),
            enabled: plugin.enabled,
        })?;
        desired.push(DesiredPlugin {
            plugin_id: plugin.plugin_id,
            source: install_source,
            revision,
            enabled: plugin.enabled,
        });
    }
    desired.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    Ok((
        DesiredState {
            schema_version: 1,
            generated_unix_seconds: now(),
            settings: config.clone(),
            plugins: desired,
        },
        warnings,
    ))
}

fn load_or_derive(
    config_dir: &Path,
    config: &Config,
    herdr_bin: &str,
    timeout: Duration,
) -> Result<(DesiredState, bool, Vec<String>), String> {
    let path = desired_path(config_dir);
    match read_regular(&path) {
        Ok(text) => {
            let desired: DesiredState = toml::from_str(&text)
                .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
            validate_desired(&desired, &path)?;
            Ok((desired, true, Vec::new()))
        }
        Err(error) if error.contains("not found") => {
            let (desired, mut warnings) = derive_local(config, herdr_bin, timeout)?;
            warnings.push(format!(
                "{} does not exist; planning from this machine's live managed plugin inventory",
                path.display()
            ));
            Ok((desired, false, warnings))
        }
        Err(error) => Err(error),
    }
}

fn validate_desired(desired: &DesiredState, path: &Path) -> Result<(), String> {
    if desired.schema_version != 1 {
        return Err(format!(
            "{} has unsupported schema_version {}",
            path.display(),
            desired.schema_version
        ));
    }
    config::validate_value(&desired.settings, path)?;
    let mut ids = BTreeSet::new();
    for plugin in &desired.plugins {
        validate_desired_plugin(plugin)?;
        if !ids.insert(plugin.plugin_id.to_ascii_lowercase()) {
            return Err(format!(
                "{} repeats plugin {}",
                path.display(),
                plugin.plugin_id
            ));
        }
    }
    Ok(())
}

fn validate_desired_plugin(plugin: &DesiredPlugin) -> Result<(), String> {
    if plugin.plugin_id.is_empty()
        || plugin.plugin_id.len() > 120
        || !plugin
            .plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-'))
    {
        return Err("desired plugin id is invalid".into());
    }
    plugins::validate_source(&plugin.source)?;
    if !plugins::full_commit(&plugin.revision) {
        return Err(format!(
            "desired plugin {} must use an exact full commit",
            plugin.plugin_id
        ));
    }
    Ok(())
}

pub fn plan(
    config_dir: &Path,
    config: &Config,
    herdr_bin: &str,
    hosts: &[String],
    timeout: Duration,
) -> Result<SyncReport, String> {
    let (desired, desired_exists, mut warnings) =
        load_or_derive(config_dir, config, herdr_bin, timeout)?;
    let (states, host_source) = fleet::collect_states(hosts, timeout);
    if host_source.is_none() {
        warnings.push("no fleet hosts file was found".into());
    }
    let local_protocol = states
        .iter()
        .find(|state| state.local)
        .and_then(|state| state.protocol);
    let mut decisions = Vec::new();
    for state in states.into_iter().filter(|state| !state.local) {
        if let Some(error) = &state.error {
            decisions.push(SyncDecision {
                host: state.host,
                target: state.target,
                plugin_id: "*".into(),
                source: None,
                previous: None,
                desired: None,
                action: SyncAction::Offline(error.clone()),
            });
            continue;
        }
        if local_protocol.is_some() && state.protocol != local_protocol {
            decisions.push(SyncDecision {
                host: state.host,
                target: state.target,
                plugin_id: "*".into(),
                source: None,
                previous: state.protocol.map(|value| value.to_string()),
                desired: local_protocol.map(|value| value.to_string()),
                action: SyncAction::Hold("protocol differs from the source host".into()),
            });
            continue;
        }
        decisions.extend(plan_host(&state, &desired));
        if desired.settings.sync_update_settings {
            let expected = config::fingerprint(&desired.settings);
            let action = match remote_fingerprint(&state.target, timeout) {
                Ok(actual) if actual == expected => SyncAction::Current,
                Ok(_) => SyncAction::SyncSettings,
                Err(error) => SyncAction::Hold(error),
            };
            decisions.push(SyncDecision {
                host: state.host,
                target: state.target,
                plugin_id: "@settings".into(),
                source: None,
                previous: None,
                desired: Some(expected),
                action,
            });
        }
    }
    decisions.sort_by(|a, b| a.host.cmp(&b.host).then(a.plugin_id.cmp(&b.plugin_id)));
    Ok(SyncReport {
        desired_path: desired_path(config_dir).display().to_string(),
        desired_exists,
        generated_unix_seconds: desired.generated_unix_seconds,
        decisions,
        warnings,
    })
}

fn plan_host(state: &HostState, desired: &DesiredState) -> Vec<SyncDecision> {
    let mut decisions = Vec::new();
    for plugin in &desired.plugins {
        let remote = state.plugins.get(&plugin.plugin_id);
        let (previous, action) = match remote {
            None => (None, SyncAction::Install),
            Some(remote) if remote.source == "local" => (
                remote.revision.clone(),
                SyncAction::Hold("remote plugin is linked; never overwrite a local fork".into()),
            ),
            Some(remote) if source_of(remote).as_deref() != Some(plugin.source.as_str()) => (
                remote.revision.clone(),
                SyncAction::Hold("remote plugin has a different source".into()),
            ),
            Some(remote) if remote.revision.as_deref() != Some(plugin.revision.as_str()) => {
                (remote.revision.clone(), SyncAction::Update)
            }
            Some(remote) if remote.enabled != plugin.enabled => (
                remote.revision.clone(),
                if plugin.enabled {
                    SyncAction::Enable
                } else {
                    SyncAction::Disable
                },
            ),
            Some(remote) => (remote.revision.clone(), SyncAction::Current),
        };
        decisions.push(SyncDecision {
            host: state.host.clone(),
            target: state.target.clone(),
            plugin_id: plugin.plugin_id.clone(),
            source: Some(plugin.source.clone()),
            previous,
            desired: Some(plugin.revision.clone()),
            action,
        });
    }
    let desired_ids: BTreeSet<&str> = desired
        .plugins
        .iter()
        .map(|plugin| plugin.plugin_id.as_str())
        .collect();
    for (plugin_id, plugin) in &state.plugins {
        if !desired_ids.contains(plugin_id.as_str()) {
            decisions.push(SyncDecision {
                host: state.host.clone(),
                target: state.target.clone(),
                plugin_id: plugin_id.clone(),
                source: source_of(plugin),
                previous: plugin.revision.clone(),
                desired: None,
                action: SyncAction::Hold(
                    "remote-only plugin; never uninstall automatically".into(),
                ),
            });
        }
    }
    decisions
}

fn source_of(plugin: &FleetPlugin) -> Option<String> {
    let (Some(owner), Some(repo)) = (&plugin.owner, &plugin.repo) else {
        return None;
    };
    let mut source = format!("{owner}/{repo}");
    if let Some(subdir) = &plugin.subdir {
        source.push('/');
        source.push_str(subdir);
    }
    plugins::validate_source(&source).ok().map(|_| source)
}

pub fn cmd_sync(
    request: CommandRequest<'_>,
    config_dir: &Path,
    config: &Config,
    herdr_bin: &str,
    timeout: Duration,
) -> i32 {
    if request.mode == "export" {
        return export_desired(config_dir, config, herdr_bin, timeout, request.json);
    }
    if !matches!(request.mode, "plan" | "apply") {
        eprintln!("herdr-updater: sync mode must be plan, apply, or export");
        return 3;
    }
    let report = match plan(config_dir, config, herdr_bin, request.hosts, timeout) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    if request.mode == "plan" || (config.policy != Policy::Auto && !request.yes) {
        print_report(&report, request.json);
        if request.mode == "apply"
            && config.policy != Policy::Auto
            && !request.yes
            && needs_change(&report)
            && !request.json
        {
            println!("notify policy: rerun with --yes or set policy = \"auto\" to reconcile");
        }
        return report_code(&report);
    }
    apply_report(report, config_dir, config, timeout, request.json)
}

fn apply_report(
    report: SyncReport,
    config_dir: &Path,
    config: &Config,
    timeout: Duration,
    json: bool,
) -> i32 {
    let desired = match read_desired_or_error(config_dir, config) {
        Ok(desired) => desired,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    let desired_by_id: BTreeMap<&str, &DesiredPlugin> = desired
        .plugins
        .iter()
        .map(|plugin| (plugin.plugin_id.as_str(), plugin))
        .collect();
    let history_path = history::path(config_dir);
    let mut changed = Vec::new();
    let mut failures = Vec::new();
    let mut blocked = report.warnings.clone();
    for decision in &report.decisions {
        let result = match decision.action {
            SyncAction::Install | SyncAction::Update | SyncAction::Enable | SyncAction::Disable => {
                let Some(plugin) = desired_by_id.get(decision.plugin_id.as_str()).copied() else {
                    failures.push(format!(
                        "{}/{}: missing desired state",
                        decision.host, decision.plugin_id
                    ));
                    continue;
                };
                apply_plugin(&decision.target, plugin, &decision.action, timeout)
            }
            SyncAction::SyncSettings => {
                apply_settings(&decision.target, &desired.settings, timeout)
            }
            SyncAction::Hold(ref reason) | SyncAction::Offline(ref reason) => {
                blocked.push(format!(
                    "{}/{}: {reason}",
                    decision.host, decision.plugin_id
                ));
                continue;
            }
            SyncAction::Current => continue,
        };
        match result {
            Ok(()) => {
                let kind = if decision.plugin_id == "@settings" {
                    EventKind::FleetConfigSynced
                } else {
                    EventKind::FleetPluginSynced
                };
                let event = Event::new(
                    kind,
                    format!("{}/{}", decision.host, decision.plugin_id),
                    decision.previous.clone().unwrap_or_else(|| "absent".into()),
                    decision.desired.clone().unwrap_or_else(|| "current".into()),
                    None,
                );
                if let Err(error) = history::append(&history_path, &event) {
                    failures.push(error);
                } else {
                    changed.push(format!("{}/{}", decision.host, decision.plugin_id));
                }
            }
            Err(error) => {
                failures.push(format!("{}/{}: {error}", decision.host, decision.plugin_id))
            }
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "changed": changed,
                "failures": failures,
                "blocked": blocked,
            }))
            .unwrap_or_else(|_| "{}".into())
        );
    } else {
        for item in &changed {
            println!("SYNCED {item}");
        }
        for error in &failures {
            println!("FAILED {error}");
        }
        for item in &blocked {
            println!("HOLD {item}");
        }
        if changed.is_empty() && failures.is_empty() && blocked.is_empty() {
            println!("fleet already matches the desired state");
        }
    }
    if !failures.is_empty()
        || report
            .decisions
            .iter()
            .any(|decision| matches!(decision.action, SyncAction::Offline(_)))
    {
        2
    } else if !blocked.is_empty() {
        1
    } else {
        0
    }
}

fn read_desired_or_error(config_dir: &Path, config: &Config) -> Result<DesiredState, String> {
    let path = desired_path(config_dir);
    let text = read_regular(&path).map_err(|_| {
        format!(
            "{} is required for apply; run `herdr-updater sync export` and review it first",
            path.display()
        )
    })?;
    let desired: DesiredState = toml::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    validate_desired(&desired, &path)?;
    if config::fingerprint(&desired.settings) != config::fingerprint(config) {
        return Err(format!(
            "{} settings differ from the active config; export and review desired state again",
            path.display()
        ));
    }
    Ok(desired)
}

fn apply_plugin(
    target: &str,
    plugin: &DesiredPlugin,
    action: &SyncAction,
    timeout: Duration,
) -> Result<(), String> {
    if !fleet::valid_alias(target) {
        return Err("remote target is invalid".into());
    }
    validate_desired_plugin(plugin)?;
    let operation = match action {
        SyncAction::Install | SyncAction::Update => {
            let enabled = if plugin.enabled { "enable" } else { "disable" };
            format!(
                "{REMOTE_HERDR} \"$H\" plugin install '{}' --ref '{}' --yes && \"$H\" plugin {enabled} '{}'",
                plugin.source, plugin.revision, plugin.plugin_id
            )
        }
        SyncAction::Enable => format!("{REMOTE_HERDR} \"$H\" plugin enable '{}'", plugin.plugin_id),
        SyncAction::Disable => format!(
            "{REMOTE_HERDR} \"$H\" plugin disable '{}'",
            plugin.plugin_id
        ),
        _ => return Ok(()),
    };
    let connect = format!("ConnectTimeout={}", timeout.as_secs().min(15));
    let out = exec::run(
        "ssh",
        &["-o", "BatchMode=yes", "-o", &connect, target, &operation],
        timeout.max(Duration::from_secs(120)),
    )
    .map_err(|error| format!("remote plugin operation: {error}"))?;
    if !out.ok() {
        return Err(format!(
            "remote plugin operation exited {}: {}",
            out.code,
            out.stderr.lines().next().unwrap_or("no stderr")
        ));
    }
    let verified = fleet::probe_remote(target, timeout);
    let actual = verified
        .plugins
        .get(&plugin.plugin_id)
        .ok_or_else(|| "plugin is absent after remote operation".to_string())?;
    if source_of(actual).as_deref() != Some(plugin.source.as_str())
        || actual.revision.as_deref() != Some(plugin.revision.as_str())
        || actual.enabled != plugin.enabled
    {
        return Err("post-sync plugin verification did not match desired state".into());
    }
    Ok(())
}

fn remote_fingerprint(target: &str, timeout: Duration) -> Result<String, String> {
    if !fleet::valid_alias(target) {
        return Err("remote target is invalid".into());
    }
    let command = format!("{REMOTE_UPDATER} \"$U\" config-fingerprint");
    let connect = format!("ConnectTimeout={}", timeout.as_secs().min(15));
    let out = exec::run(
        "ssh",
        &["-o", "BatchMode=yes", "-o", &connect, target, &command],
        timeout,
    )
    .map_err(|error| format!("remote settings check: {error}"))?;
    if !out.ok() {
        return Err("remote updater is unavailable for settings sync".into());
    }
    let value = out.trimmed();
    if value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err("remote updater returned an invalid settings fingerprint".into())
    }
}

fn apply_settings(target: &str, config: &Config, timeout: Duration) -> Result<(), String> {
    if !fleet::valid_alias(target) {
        return Err("remote target is invalid".into());
    }
    let payload = toml::to_string_pretty(config)
        .map_err(|error| format!("cannot encode synced settings: {error}"))?;
    let connect = format!("ConnectTimeout={}", timeout.as_secs().min(15));
    let out = exec::run_with_input(
        "ssh",
        &[
            "-o",
            "BatchMode=yes",
            "-o",
            &connect,
            target,
            REMOTE_CONFIG_APPLY,
        ],
        payload.as_bytes(),
        timeout,
    )
    .map_err(|error| format!("remote settings sync: {error}"))?;
    if !out.ok() {
        return Err(format!("remote settings sync exited {}", out.code));
    }
    let actual = remote_fingerprint(target, timeout)?;
    let expected = config::fingerprint(config);
    if actual != expected {
        return Err("remote settings fingerprint did not match after sync".into());
    }
    Ok(())
}

fn print_report(report: &SyncReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!(
        "fleet sync plan from {}{}",
        report.desired_path,
        if report.desired_exists {
            ""
        } else {
            " (live local state)"
        }
    );
    for warning in &report.warnings {
        println!("HOLD    {warning}");
    }
    for decision in &report.decisions {
        let action = match &decision.action {
            SyncAction::Current => "CURRENT".into(),
            SyncAction::Install => "INSTALL".into(),
            SyncAction::Update => "UPDATE".into(),
            SyncAction::Enable => "ENABLE".into(),
            SyncAction::Disable => "DISABLE".into(),
            SyncAction::SyncSettings => "SETTINGS".into(),
            SyncAction::Hold(reason) => format!("HOLD -- {reason}"),
            SyncAction::Offline(reason) => format!("OFFLINE -- {reason}"),
        };
        println!(
            "  {:<16} {:<28} {}",
            decision.host, decision.plugin_id, action
        );
    }
}

fn needs_change(report: &SyncReport) -> bool {
    report.decisions.iter().any(|decision| {
        matches!(
            decision.action,
            SyncAction::Install
                | SyncAction::Update
                | SyncAction::Enable
                | SyncAction::Disable
                | SyncAction::SyncSettings
        )
    })
}

fn report_code(report: &SyncReport) -> i32 {
    if report
        .decisions
        .iter()
        .any(|decision| matches!(decision.action, SyncAction::Offline(_)))
    {
        2
    } else if !report.warnings.is_empty()
        || needs_change(report)
        || report
            .decisions
            .iter()
            .any(|decision| matches!(decision.action, SyncAction::Hold(_)))
    {
        1
    } else {
        0
    }
}

fn read_regular(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("{} not found", path.display())
        } else {
            format!("cannot inspect {}: {error}", path.display())
        }
    })?;
    if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
        return Err(format!("{} is not a bounded regular file", path.display()));
    }
    fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn write_regular(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > 1024 * 1024 {
        return Err("desired state exceeds the size limit".into());
    }
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired() -> DesiredState {
        DesiredState {
            schema_version: 1,
            generated_unix_seconds: 1,
            settings: Config::default(),
            plugins: vec![DesiredPlugin {
                plugin_id: "sample".into(),
                source: "diegopzz/herdr-sample".into(),
                revision: "a".repeat(40),
                enabled: true,
            }],
        }
    }

    fn host(plugin: Option<FleetPlugin>) -> HostState {
        let mut plugins = BTreeMap::new();
        if let Some(plugin) = plugin {
            plugins.insert("sample".into(), plugin);
        }
        HostState {
            host: "laptop".into(),
            target: "laptop".into(),
            local: false,
            version: Some("0.8.2".into()),
            protocol: Some(20),
            server_running: true,
            plugins,
            plugin_error: None,
            error: None,
        }
    }

    #[test]
    fn missing_plugin_is_an_install() {
        let decisions = plan_host(&host(None), &desired());
        assert!(matches!(decisions[0].action, SyncAction::Install));
    }

    #[test]
    fn linked_remote_plugin_is_held() {
        let decisions = plan_host(
            &host(Some(FleetPlugin {
                version: Some("1.0.0".into()),
                source: "local".into(),
                revision: None,
                owner: None,
                repo: None,
                subdir: None,
                requested_ref: None,
                enabled: true,
            })),
            &desired(),
        );
        assert!(matches!(decisions[0].action, SyncAction::Hold(_)));
    }

    #[test]
    fn same_source_with_different_revision_updates() {
        let decisions = plan_host(
            &host(Some(FleetPlugin {
                version: Some("0.9.0".into()),
                source: "github".into(),
                revision: Some("b".repeat(40)),
                owner: Some("diegopzz".into()),
                repo: Some("herdr-sample".into()),
                subdir: None,
                requested_ref: None,
                enabled: true,
            })),
            &desired(),
        );
        assert!(matches!(decisions[0].action, SyncAction::Update));
    }

    #[test]
    fn desired_state_requires_exact_commits() {
        let mut state = desired();
        state.plugins[0].revision = "main".into();
        assert!(validate_desired(&state, Path::new("desired.toml")).is_err());
    }

    #[test]
    fn warnings_make_a_sync_incomplete() {
        let report = SyncReport {
            desired_path: "desired.toml".into(),
            desired_exists: true,
            generated_unix_seconds: 0,
            decisions: Vec::new(),
            warnings: vec!["no connected hosts".into()],
        };
        assert_eq!(report_code(&report), 1);
    }
}
