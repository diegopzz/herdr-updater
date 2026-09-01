//! herdr *core* — version and protocol, not plugins.
//!
//! Plugin updates are the easy half. herdr itself is the component whose
//! version actually breaks things: the wire protocol lives here, and a
//! protocol mismatch takes out `herdr --remote` between two hosts at once.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::exec;
use crate::version;

/// The channel that <https://herdr.dev/latest.json> describes. A client that
/// tracks anything else is being compared against a manifest that does not
/// describe it, which is not a comparison at all.
pub const MANIFEST_CHANNEL: &str = "stable";

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

/// How the installed core relates to the release manifest.
///
/// Plugins have had this since the first version — `Behind` is the only
/// relation that may be applied, and `Ahead`/`Diverged`/`Unknown` are held.
/// Core used to compare with `!=`, which collapsed all four into "update", so
/// a machine running something newer than stable would be *downgraded* by an
/// unattended `auto` run. Same shape, same rule, same reason.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoreRelation {
    /// Installed and published versions are the same.
    Same,
    /// The manifest is newer: the only relation that may be applied.
    Behind,
    /// The installed core is newer than the manifest.
    Ahead,
    /// At least one side could not be ordered, so there is no relation.
    Unknown,
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
    /// How `installed` orders against `latest`. `update_available` is exactly
    /// `relation == Behind`; it is never inferred from inequality.
    pub relation: CoreRelation,
    /// True when the client tracks a channel this manifest does not describe.
    pub channel_mismatch: bool,
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
            relation: CoreRelation::Unknown,
            channel_mismatch: false,
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
        relation: CoreRelation::Unknown,
        channel_mismatch: false,
        update_available: false,
        protocol_change: false,
        error: None,
    };
    match manifest(timeout) {
        Ok(m) => {
            state.relation = relate(&st.client.version, &m.version);
            state.update_available = state.relation == CoreRelation::Behind;
            state.channel_mismatch = st
                .client
                .channel
                .as_deref()
                .is_some_and(|channel| !channel.eq_ignore_ascii_case(MANIFEST_CHANNEL));
            state.protocol_change = m.protocol != st.client.protocol;
            state.latest = Some(m.version);
            state.latest_protocol = Some(m.protocol);
        }
        Err(e) => state.error = Some(e),
    }
    state
}

/// Order the installed version against the published one.
fn relate(installed: &str, latest: &str) -> CoreRelation {
    match version::compare(installed, latest) {
        Some(std::cmp::Ordering::Equal) => CoreRelation::Same,
        Some(std::cmp::Ordering::Less) => CoreRelation::Behind,
        Some(std::cmp::Ordering::Greater) => CoreRelation::Ahead,
        None => CoreRelation::Unknown,
    }
}

/// Why an update is being held back. Returned instead of a bare bool so the
/// report can say *which* guard fired — "held" with no reason is the thing
/// that makes people disable a tool.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "hold")]
pub enum Hold {
    /// Already current.
    UpToDate,
    /// The installed core is newer than the published one. Applying here would
    /// be a downgrade, and a downgrade is never what "keep me current" meant.
    LocalIsAhead,
    /// One of the two versions does not parse, so there is no ordering to act
    /// on. Reported separately from a failed fetch because the remedy differs:
    /// this is a version string we do not understand, not a network we could
    /// not reach.
    Unorderable,
    /// The client tracks a channel the manifest does not describe, so "latest"
    /// is latest for somebody else.
    ChannelMismatch(String),
    /// A server is running but this build cannot hand off, so updating would
    /// drop every attached client and every pane process with it.
    NoLiveHandoff,
    /// The check itself failed; we do not act on an unknown.
    Unknown(String),
}

/// One sentence per hold, written for the person reading the report. Held with
/// no reason is the thing that makes people disable a tool, so every variant
/// says what fired and what would clear it.
pub fn hold_text(hold: &Hold) -> String {
    match hold {
        Hold::UpToDate => "already current".into(),
        Hold::LocalIsAhead => {
            "installed core is newer than the release manifest; refusing to downgrade".into()
        }
        Hold::Unorderable => {
            "installed and published versions cannot be ordered; not treating that as current"
                .into()
        }
        Hold::ChannelMismatch(channel) => format!(
            "client tracks the {channel:?} channel but latest.json describes {MANIFEST_CHANNEL:?}"
        ),
        Hold::NoLiveHandoff => "running server cannot live-handoff".into(),
        Hold::Unknown(error) => error.clone(),
    }
}

/// Decide whether herdr core may be updated on this host, on its own merits.
/// Fleet-level protocol staging is layered on top of this in `fleet`.
pub fn gate(state: &CoreState, allow_channel_mismatch: bool) -> Result<(), Hold> {
    if let Some(e) = &state.error {
        return Err(Hold::Unknown(e.clone()));
    }
    match state.relation {
        // Checked before `UpToDate`: an unorderable pair is not "no update
        // needed", and reporting it as green is the failure this crate exists
        // to prevent.
        CoreRelation::Unknown => return Err(Hold::Unorderable),
        CoreRelation::Ahead => return Err(Hold::LocalIsAhead),
        CoreRelation::Same => return Err(Hold::UpToDate),
        CoreRelation::Behind => {}
    }
    if state.channel_mismatch && !allow_channel_mismatch {
        return Err(Hold::ChannelMismatch(
            state.channel.clone().unwrap_or_else(|| "unknown".into()),
        ));
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
    allow_channel_mismatch: bool,
    timeout: Duration,
) -> Result<ApplyResult, String> {
    gate(before, allow_channel_mismatch).map_err(|hold| hold_text(&hold))?;
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
            relation: latest.map_or(CoreRelation::Unknown, |l| relate(installed, l)),
            channel_mismatch: false,
            update_available: latest.is_some_and(|l| relate(installed, l) == CoreRelation::Behind),
            protocol_change: false,
            error: None,
        }
    }

    #[test]
    fn up_to_date_is_held() {
        assert_eq!(
            gate(&state("0.8.2", Some("0.8.2"), true, true), false),
            Err(Hold::UpToDate)
        );
    }

    #[test]
    fn newer_upstream_with_handoff_is_allowed() {
        assert_eq!(
            gate(&state("0.8.2", Some("0.9.0"), true, true), false),
            Ok(())
        );
    }

    #[test]
    fn running_server_without_live_handoff_is_held() {
        assert_eq!(
            gate(&state("0.8.2", Some("0.9.0"), true, false), false),
            Err(Hold::NoLiveHandoff)
        );
    }

    #[test]
    fn no_server_running_needs_no_handoff() {
        assert_eq!(
            gate(&state("0.8.2", Some("0.9.0"), false, false), false),
            Ok(())
        );
    }

    #[test]
    fn a_failed_check_never_becomes_an_update() {
        let mut s = state("0.8.2", None, true, true);
        s.error = Some("network down".into());
        assert!(matches!(gate(&s, false), Err(Hold::Unknown(_))));
    }

    #[test]
    fn an_installed_core_ahead_of_the_manifest_is_never_downgraded() {
        // A rolled-back release, or a host deliberately running ahead. String
        // inequality called this an update; ordering calls it what it is.
        let ahead = state("0.9.0", Some("0.8.2"), true, true);
        assert_eq!(ahead.relation, CoreRelation::Ahead);
        assert!(!ahead.update_available);
        assert_eq!(gate(&ahead, false), Err(Hold::LocalIsAhead));
    }

    #[test]
    fn a_prerelease_client_is_ahead_of_the_release_it_leads_to() {
        assert_eq!(
            relate("0.9.0-rc.1", "0.8.2"),
            CoreRelation::Ahead,
            "a release candidate is still newer than the last stable"
        );
        assert_eq!(relate("0.9.0-rc.1", "0.9.0"), CoreRelation::Behind);
    }

    #[test]
    fn text_versions_are_unorderable_rather_than_an_update() {
        let odd = state("nightly-20260830", Some("0.9.0"), true, true);
        assert_eq!(odd.relation, CoreRelation::Unknown);
        assert!(!odd.update_available);
        assert_eq!(gate(&odd, false), Err(Hold::Unorderable));
    }

    #[test]
    fn a_channel_the_manifest_does_not_describe_is_held_until_opted_in() {
        let mut s = state("0.8.2", Some("0.9.0"), false, false);
        s.channel = Some("nightly".into());
        s.channel_mismatch = true;
        assert!(matches!(gate(&s, false), Err(Hold::ChannelMismatch(c)) if c == "nightly"));
        assert_eq!(gate(&s, true), Ok(()));
    }

    #[test]
    fn version_ordering_drives_update_available_not_inequality() {
        // 0.10.0 is newer than 0.9.0 despite sorting before it as text.
        assert!(state("0.9.0", Some("0.10.0"), false, false).update_available);
        assert!(!state("0.10.0", Some("0.9.0"), false, false).update_available);
    }

    #[test]
    fn parses_only_outdated_installed_integrations() {
        let output = "codex: outdated (v7 -> v8)\nclaude: current (v8)\npi: not installed\n";
        assert_eq!(outdated_integrations(output), vec!["codex"]);
    }
}
