//! herdr *core* — version and protocol, not plugins.
//!
//! Plugin updates are the easy half. herdr itself is the component whose
//! version actually breaks things: the wire protocol lives here, and a
//! protocol mismatch takes out `herdr --remote` between two hosts at once.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::exec;

/// Shape of `herdr status --json`. Only the fields we act on are named; herdr
/// is free to add more without breaking us (serde ignores unknown keys).
#[derive(Debug, Clone, Deserialize)]
pub struct StatusJson {
    pub client: ClientStatus,
    pub server: Option<ServerStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientStatus {
    pub version: String,
    #[serde(default)]
    pub channel: Option<String>,
    pub protocol: u32,
    #[serde(default)]
    pub binary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerStatus {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub protocol: Option<u32>,
    #[serde(default)]
    pub compatible: Option<bool>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub restart_needed: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capabilities {
    /// When false, `herdr update --handoff` cannot hand off and the update
    /// becomes disruptive: every attached client is dropped. We refuse to
    /// update a *running* server without this rather than surprise anyone
    /// mid-session.
    #[serde(default)]
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
}

/// Shape of <https://herdr.dev/latest.json> — the same manifest `herdr update`
/// itself reads, so we can never disagree with it about what "latest" means.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub protocol: u32,
}

/// What we found out about herdr on one host.
#[derive(Debug, Clone, Serialize)]
pub struct CoreState {
    pub installed: String,
    pub channel: Option<String>,
    pub binary: Option<String>,
    pub protocol: u32,
    pub server_status: Option<String>,
    pub server_running: bool,
    pub server_version: Option<String>,
    pub server_protocol: Option<u32>,
    pub compatible: Option<bool>,
    pub live_handoff: bool,
    pub detached_server_daemon: bool,
    pub restart_needed: bool,
    pub latest: Option<String>,
    pub latest_protocol: Option<u32>,
    pub update_available: bool,
    /// True when updating would move the wire protocol. Cosmetic on one host;
    /// on a fleet it is the difference between a version bump and hosts that
    /// can no longer attach to each other with `herdr --remote`.
    pub protocol_change: bool,
    pub error: Option<String>,
}

impl CoreState {
    fn errored(msg: String) -> Self {
        CoreState {
            installed: String::new(),
            channel: None,
            binary: None,
            protocol: 0,
            server_status: None,
            server_running: false,
            server_version: None,
            server_protocol: None,
            compatible: None,
            live_handoff: false,
            detached_server_daemon: false,
            restart_needed: false,
            latest: None,
            latest_protocol: None,
            update_available: false,
            protocol_change: false,
            error: Some(msg),
        }
    }
}

/// Read `herdr status --json` from whichever herdr is on PATH.
pub fn status(herdr_bin: &str, timeout: Duration) -> Result<StatusJson, String> {
    let out = exec::run(herdr_bin, &["status", "--json"], timeout)
        .map_err(|e| format!("herdr status: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "herdr status exited {}: {}",
            out.code,
            out.stderr.trim().lines().next().unwrap_or("(no stderr)")
        ));
    }
    serde_json::from_str(&out.stdout).map_err(|e| format!("herdr status --json is not JSON: {e}"))
}

/// Fetch the release manifest. Shelling out to curl keeps this binary free of
/// a TLS stack — which is what lets the release artifacts stay small enough to
/// download on first use.
pub fn manifest(timeout: Duration) -> Result<Manifest, String> {
    if !exec::have("curl") {
        return Err("curl is not on PATH — cannot read herdr.dev/latest.json".into());
    }
    let out = exec::run(
        "curl",
        &["-fsSL", "--max-time", "20", "https://herdr.dev/latest.json"],
        timeout,
    )
    .map_err(|e| format!("fetching latest.json: {e}"))?;
    if !out.ok() {
        return Err(format!("latest.json fetch exited {}", out.code));
    }
    serde_json::from_str(&out.stdout).map_err(|e| format!("latest.json is not JSON: {e}"))
}

/// Combine local status with the upstream manifest into one verdict.
///
/// A manifest fetch failure is *not* fatal: we still report what is installed,
/// with `update_available = false`. Reporting "no update" on a dead network is
/// the safe degradation; claiming an update exists when we could not check
/// would be the dangerous one.
pub fn inspect(herdr_bin: &str, timeout: Duration) -> CoreState {
    let st = match status(herdr_bin, timeout) {
        Ok(s) => s,
        Err(e) => return CoreState::errored(e),
    };
    let server = st.server.clone();
    let mut state = CoreState {
        installed: st.client.version.clone(),
        channel: st.client.channel.clone(),
        binary: st.client.binary.clone(),
        protocol: st.client.protocol,
        server_status: server.as_ref().and_then(|s| s.status.clone()),
        server_running: server.as_ref().map(|s| s.running).unwrap_or(false),
        server_version: server.as_ref().and_then(|s| s.version.clone()),
        server_protocol: server.as_ref().and_then(|s| s.protocol),
        compatible: server.as_ref().and_then(|s| s.compatible),
        live_handoff: server
            .as_ref()
            .map(|s| s.capabilities.live_handoff)
            .unwrap_or(false),
        detached_server_daemon: server
            .as_ref()
            .map(|s| s.capabilities.detached_server_daemon)
            .unwrap_or(false),
        restart_needed: server.as_ref().map(|s| s.restart_needed).unwrap_or(false),
        latest: None,
        latest_protocol: None,
        update_available: false,
        protocol_change: false,
        error: None,
    };
    match manifest(timeout) {
        Ok(m) => {
            state.update_available = m.version != st.client.version;
            state.protocol_change = m.protocol != st.client.protocol;
            state.latest = Some(m.version);
            state.latest_protocol = Some(m.protocol);
        }
        Err(e) => state.error = Some(e),
    }
    state
}

/// Why an update is being held back. Returned instead of a bare bool so the
/// report can say *which* guard fired — "held" with no reason is the thing
/// that makes people disable a tool.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "hold")]
pub enum Hold {
    /// Already current.
    UpToDate,
    /// A server is running but this build cannot hand off, so updating would
    /// drop every attached client and every pane process with it.
    NoLiveHandoff,
    /// The check itself failed; we do not act on an unknown.
    Unknown(String),
}

/// Decide whether herdr core may be updated on this host, on its own merits.
/// Fleet-level protocol staging is layered on top of this in `fleet`.
pub fn gate(state: &CoreState) -> Result<(), Hold> {
    if let Some(e) = &state.error {
        return Err(Hold::Unknown(e.clone()));
    }
    if !state.update_available {
        return Err(Hold::UpToDate);
    }
    if state.server_running && !state.live_handoff {
        return Err(Hold::NoLiveHandoff);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub installed: String,
    pub integrations_refreshed: Vec<String>,
}

fn outdated_integrations(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (name, status) = line.split_once(':')?;
            let name = name.trim();
            let status = status.trim().to_ascii_lowercase();
            let valid_name = !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
            (valid_name && !status.contains("current") && !status.contains("not installed"))
                .then(|| name.to_string())
        })
        .collect()
}

fn refresh_integrations(herdr_bin: &str, timeout: Duration) -> Result<Vec<String>, String> {
    let out = exec::run(
        herdr_bin,
        &["integration", "status", "--outdated-only"],
        timeout,
    )
    .map_err(|e| format!("integration status: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "integration status exited {}: {}",
            out.code,
            out.stderr.lines().next().unwrap_or("no stderr")
        ));
    }
    let agents = outdated_integrations(&out.stdout);
    for agent in &agents {
        let installed = exec::run(herdr_bin, &["integration", "install", agent], timeout)
            .map_err(|e| format!("integration install {agent}: {e}"))?;
        if !installed.ok() {
            return Err(format!(
                "integration install {agent} exited {}: {}",
                installed.code,
                installed.stderr.lines().next().unwrap_or("no stderr")
            ));
        }
    }
    Ok(agents)
}

pub fn apply(
    herdr_bin: &str,
    before: &CoreState,
    timeout: Duration,
) -> Result<ApplyResult, String> {
    gate(before).map_err(|hold| match hold {
        Hold::UpToDate => "herdr is already current".to_string(),
        Hold::NoLiveHandoff => "running server does not support live handoff".to_string(),
        Hold::Unknown(error) => error,
    })?;
    let expected = before
        .latest
        .as_deref()
        .ok_or_else(|| "latest herdr version is unknown".to_string())?;
    let out = exec::run(herdr_bin, &["update", "--handoff"], timeout)
        .map_err(|e| format!("herdr update --handoff: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "herdr update --handoff exited {}: {}",
            out.code,
            out.stderr.lines().next().unwrap_or("no stderr")
        ));
    }

    let after = status(herdr_bin, timeout)?;
    if after.client.version != expected {
        return Err(format!(
            "post-update client is {}, expected {expected}",
            after.client.version
        ));
    }
    if before.server_running {
        let server = after
            .server
            .filter(|server| server.running)
            .ok_or_else(|| {
                "server was running before update but is not running after handoff".to_string()
            })?;
        if server.version.as_deref() != Some(expected) {
            return Err(format!(
                "post-handoff server is {}, expected {expected}",
                server.version.as_deref().unwrap_or("unknown")
            ));
        }
    }
    let integrations_refreshed = refresh_integrations(herdr_bin, timeout)?;
    let verify = exec::run(
        herdr_bin,
        &["integration", "status", "--outdated-only"],
        timeout,
    )
    .map_err(|e| format!("integration verification: {e}"))?;
    if !verify.ok() || !outdated_integrations(&verify.stdout).is_empty() {
        return Err("one or more Herdr integrations remain outdated".into());
    }
    Ok(ApplyResult {
        installed: expected.to_string(),
        integrations_refreshed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured verbatim from `herdr status --json` on vspc-wsl, 2026-08-30.
    const REAL: &str = r#"{"client":{"version":"0.8.2","channel":"stable","protocol":20,"binary":"/root/.local/bin/herdr","session":null},"server":{"status":"running","running":true,"version":"0.8.2","protocol":20,"capabilities":{"live_handoff":true,"detached_server_daemon":true},"compatible":true,"socket":"/root/.config/herdr/herdr.sock","session":null,"restart_needed":false},"update":{"restart_needed":false}}"#;

    fn parsed() -> StatusJson {
        serde_json::from_str(REAL).expect("real herdr output must parse")
    }

    #[test]
    fn parses_real_herdr_status() {
        let s = parsed();
        assert_eq!(s.client.version, "0.8.2");
        assert_eq!(s.client.protocol, 20);
        let srv = s.server.unwrap();
        assert!(srv.running);
        assert!(srv.capabilities.live_handoff);
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // herdr must be free to add keys without breaking every installed copy
        // of this tool.
        let with_extra = REAL.replace(r#""client":{"#, r#""client":{"brand_new_key":1,"#);
        assert!(serde_json::from_str::<StatusJson>(&with_extra).is_ok());
    }

    fn state(installed: &str, latest: Option<&str>, running: bool, handoff: bool) -> CoreState {
        CoreState {
            installed: installed.into(),
            channel: Some("stable".into()),
            binary: None,
            protocol: 20,
            server_status: Some("running".into()),
            server_running: running,
            server_version: Some(installed.into()),
            server_protocol: Some(20),
            compatible: Some(true),
            live_handoff: handoff,
            detached_server_daemon: true,
            restart_needed: false,
            latest: latest.map(|s| s.to_string()),
            latest_protocol: Some(20),
            update_available: latest.map(|l| l != installed).unwrap_or(false),
            protocol_change: false,
            error: None,
        }
    }

    #[test]
    fn up_to_date_is_held() {
        assert_eq!(
            gate(&state("0.8.2", Some("0.8.2"), true, true)),
            Err(Hold::UpToDate)
        );
    }

    #[test]
    fn newer_upstream_with_handoff_is_allowed() {
        assert_eq!(gate(&state("0.8.2", Some("0.9.0"), true, true)), Ok(()));
    }

    #[test]
    fn running_server_without_live_handoff_is_held() {
        assert_eq!(
            gate(&state("0.8.2", Some("0.9.0"), true, false)),
            Err(Hold::NoLiveHandoff)
        );
    }

    #[test]
    fn no_server_running_needs_no_handoff() {
        assert_eq!(gate(&state("0.8.2", Some("0.9.0"), false, false)), Ok(()));
    }

    #[test]
    fn a_failed_check_never_becomes_an_update() {
        let mut s = state("0.8.2", None, true, true);
        s.error = Some("network down".into());
        assert!(matches!(gate(&s), Err(Hold::Unknown(_))));
    }

    #[test]
    fn parses_only_outdated_installed_integrations() {
        let output = "codex: outdated (v7 -> v8)\nclaude: current (v8)\npi: not installed\n";
        assert_eq!(outdated_integrations(output), vec!["codex"]);
    }
}
