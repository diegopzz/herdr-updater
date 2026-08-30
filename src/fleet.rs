//! Fleet mode — the reason this project exists rather than a PR to the
//! reference.
//!
//! A host-local updater cannot see the failure that actually costs you a day:
//! several machines quietly running different versions, each of them
//! individually healthy and each of them reporting green.
//!
//! So the primary output here is not "updates available". It is **drift**.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::exec;
use crate::herdr;

/// Hosts come from our own config if present, otherwise from herdr-mirror's —
/// which most people who need this tool already maintain. Sharing that file
/// means the fleet is defined once, and a host added for mirroring is
/// automatically a host we keep updated.
const CONFIG_CANDIDATES: [&str; 2] =
    [".config/herdr-updater/hosts.toml", ".config/herdr-mirror/hosts.toml"];

#[derive(Debug, Deserialize, Default)]
struct HostsFile {
    #[serde(default)]
    hosts: BTreeMap<String, HostEntry>,
}

#[derive(Debug, Deserialize, Default)]
struct HostEntry {
    /// The ssh alias. Defaults to the table key, matching herdr-mirror, so
    /// `[hosts.macbook]` with no `target` means `ssh macbook`.
    #[serde(default)]
    target: Option<String>,
}

/// One row of the drift report.
#[derive(Debug, Clone, Serialize)]
pub struct HostState {
    pub host: String,
    pub target: String,
    pub local: bool,
    pub version: Option<String>,
    pub protocol: Option<u32>,
    pub server_running: bool,
    pub error: Option<String>,
}

/// An ssh alias is interpolated into an argv we hand to `ssh`. It comes from a
/// config file rather than the network, but "config file" is not "trusted
/// input" — reject anything that is not plainly a host alias so a stray value
/// can never grow into an option or a second argument.
fn valid_alias(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Load the fleet definition, returning the file we actually read so the
/// report can say which config won. "Which config won?" should never be a
/// guess — that ambiguity is its own class of bug.
fn load_hosts() -> (Vec<(String, String)>, Option<PathBuf>) {
    let Some(home) = home() else { return (vec![], None) };
    for candidate in CONFIG_CANDIDATES {
        let path = home.join(candidate);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(parsed) = toml::from_str::<HostsFile>(&text) else { continue };
        let hosts = parsed
            .hosts
            .into_iter()
            .map(|(name, e)| {
                let target = e.target.unwrap_or_else(|| name.clone());
                (name, target)
            })
            .collect();
        return (hosts, Some(path));
    }
    (vec![], None)
}

/// `herdr status --json` on a remote host.
///
/// The remote binary is resolved the same way herdr-mirror resolves it —
/// PATH first, then `~/.local/bin/herdr` — because a non-login ssh session
/// frequently has neither `~/.local/bin` on PATH nor a profile that adds it.
/// Getting this wrong is exactly the failure that makes a healthy host look
/// like it has no herdr at all.
fn probe_remote(target: &str, timeout: Duration) -> HostState {
    let mut st = HostState {
        host: target.to_string(),
        target: target.to_string(),
        local: false,
        version: None,
        protocol: None,
        server_running: false,
        error: None,
    };
    if !valid_alias(target) {
        st.error = Some(format!("refusing to ssh to an implausible alias: {target:?}"));
        return st;
    }
    let remote = "if command -v herdr >/dev/null 2>&1; then herdr status --json; \
                  elif [ -x \"$HOME/.local/bin/herdr\" ]; then \"$HOME/.local/bin/herdr\" status --json; \
                  else echo NO_HERDR; fi";
    let connect = format!("ConnectTimeout={}", timeout.as_secs().min(15));
    let out = exec::run("ssh", &["-o", "BatchMode=yes", "-o", &connect, target, remote], timeout);
    match out {
        Err(e) => st.error = Some(e.to_string()),
        Ok(o) if o.stdout.contains("NO_HERDR") => {
            // Worth saying precisely: ssh worked, herdr is simply not there.
            // Reading that as "connection failed" sends people to debug a
            // network that is fine — the exact confusion that makes
            // herdr-mirror's "add a machine" look like it fails silently.
            st.error = Some("ssh ok, but herdr is not installed on this host".into());
        }
        Ok(o) if !o.ok() => {
            st.error = Some(format!(
                "ssh exited {}: {}",
                o.code,
                o.stderr.trim().lines().next().unwrap_or("(no stderr)")
            ));
        }
        Ok(o) => match serde_json::from_str::<herdr::StatusJson>(o.stdout.trim()) {
            Ok(s) => {
                st.version = Some(s.client.version);
                st.protocol = Some(s.client.protocol);
                st.server_running = s.server.map(|x| x.running).unwrap_or(false);
            }
            Err(e) => st.error = Some(format!("unparseable status: {e}")),
        },
    }
    st
}

fn probe_local(timeout: Duration) -> HostState {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let mut st = HostState {
        host: "this machine".into(),
        target: "local".into(),
        local: true,
        version: None,
        protocol: None,
        server_running: false,
        error: None,
    };
    match herdr::status(&bin, timeout) {
        Ok(s) => {
            st.version = Some(s.client.version);
            st.protocol = Some(s.client.protocol);
            st.server_running = s.server.map(|x| x.running).unwrap_or(false);
        }
        Err(e) => st.error = Some(e),
    }
    st
}

/// Fan out over hosts. Bounded so a large fleet cannot open an unbounded
/// number of ssh connections at once; wall-clock cost stays roughly one round
/// trip regardless of host count.
const MAX_CONCURRENCY: usize = 8;

/// Deliberately narrow about blast radius. An earlier draft of this warning
/// said a protocol split stops mirrors working. That is wrong, and measuring
/// it is what caught it: on 2026-08-30 a protocol 18 host mirrored cleanly
/// from a protocol 20 host, because herdr-mirror runs the *remote* host's own
/// herdr binary, so both ends of that conversation already agree. A warning
/// that overstates the damage gets disabled, and then it protects nothing.
const SPLIT_WARNING: &str = "  \u{26a0} PROTOCOL SPLIT — hosts are on different herdr protocols.
    `herdr --remote` negotiates on this, so a local client cannot attach to a
    remote server across the split.
    herdr-mirror is NOT affected the same way: it runs the remote host's own
    herdr binary, so both ends of that conversation already match.
    Update before relying on --remote, and close the drift regardless.";

pub fn cmd_fleet(only: &[String], timeout: Duration, json: bool) -> i32 {
    let (mut hosts, source) = load_hosts();
    if !only.is_empty() {
        hosts.retain(|(name, target)| only.contains(name) || only.contains(target));
    }

    let (tx, rx) = mpsc::channel();
    let mut states = vec![probe_local(timeout)];

    for chunk in hosts.chunks(MAX_CONCURRENCY) {
        let mut handles = Vec::new();
        for (name, target) in chunk {
            let (name, target, tx) = (name.clone(), target.clone(), tx.clone());
            handles.push(thread::spawn(move || {
                let mut st = probe_remote(&target, timeout);
                st.host = name;
                let _ = tx.send(st);
            }));
        }
        for h in handles {
            let _ = h.join();
        }
    }
    drop(tx);
    states.extend(rx.into_iter());
    states.sort_by(|a, b| b.local.cmp(&a.local).then(a.host.cmp(&b.host)));

    // Drift is computed only over hosts we could actually read. An unreachable
    // host is reported as unknown, never quietly folded into "the fleet
    // agrees" — that is the exact failure this tool exists to prevent.
    let protocols: std::collections::BTreeSet<u32> =
        states.iter().filter_map(|s| s.protocol).collect();
    let versions: std::collections::BTreeSet<&str> =
        states.iter().filter_map(|s| s.version.as_deref()).collect();
    let protocol_split = protocols.len() > 1;
    let version_split = versions.len() > 1;
    let unreachable = states.iter().filter(|s| s.error.is_some()).count();

    if json {
        let doc = serde_json::json!({
            "config": source.as_ref().map(|p| p.display().to_string()),
            "hosts": states,
            "protocol_split": protocol_split,
            "version_split": version_split,
            "unreachable": unreachable,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into()));
    } else {
        match &source {
            Some(p) => println!("fleet (from {})", p.display()),
            None => println!(
                "fleet (no hosts.toml found — checked ~/{} and ~/{}; showing this machine only)",
                CONFIG_CANDIDATES[0], CONFIG_CANDIDATES[1]
            ),
        }
        println!();
        println!("  {:<16} {:<14} {:<9} {:<9}", "HOST", "HERDR", "PROTOCOL", "SERVER");
        for s in &states {
            let server = if s.error.is_some() {
                "-"
            } else if s.server_running {
                "running"
            } else {
                "stopped"
            };
            // A long preview version string must not push the columns apart;
            // the full value is always in --json.
            let version = match s.version.as_deref() {
                Some(v) if v.chars().count() > 14 => {
                    format!("{}\u{2026}", v.chars().take(13).collect::<String>())
                }
                Some(v) => v.to_string(),
                None => "?".into(),
            };
            println!(
                "  {:<16} {:<14} {:<9} {:<9}{}",
                s.host,
                version,
                s.protocol.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                server,
                s.error.as_deref().map(|e| format!("  {e}")).unwrap_or_default(),
            );
        }
        println!();
        if protocol_split {
            println!("{SPLIT_WARNING}");
        } else if version_split {
            println!(
                "  version drift, but the protocol matches on every host we could read —\n    \
                 mirrors and --remote keep working; update at your convenience."
            );
        } else if unreachable == 0 {
            println!("  fleet agrees: one version, one protocol.");
        }
        if unreachable > 0 {
            println!(
                "  {unreachable} host(s) could not be read — excluded from the verdict above, \
                 not assumed to agree."
            );
        }
    }

    if unreachable > 0 {
        2
    } else if protocol_split || version_split {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_aliases_that_could_become_ssh_options_or_arguments() {
        assert!(valid_alias("ts-ubuntu"));
        assert!(valid_alias("portatil-wsl"));
        assert!(valid_alias("diego@macbook"));
        assert!(!valid_alias("-oProxyCommand=touch /tmp/pwned"));
        assert!(!valid_alias("host; rm -rf /"));
        assert!(!valid_alias("host with space"));
        assert!(!valid_alias(""));
    }

    #[test]
    fn hosts_file_defaults_target_to_the_table_key() {
        let f: HostsFile = toml::from_str("[hosts.macbook]\n").unwrap();
        assert_eq!(f.hosts["macbook"].target, None, "absent target means: use the key");
    }

    #[test]
    fn reads_the_herdr_mirror_hosts_shape_unchanged() {
        // Verbatim shape from ~/.config/herdr-mirror/hosts.toml, extra keys and
        // all — sharing that file is the point, so unknown keys must not break.
        let f: HostsFile = toml::from_str(
            r#"
poll_seconds = 20
default_host = "macbook"
close_remote_on_local_close = "panes"
[hosts.macbook]
target = "macbook"
always_control = false
[hosts.ts-ubuntu]
target = "ts-ubuntu"
always_control = true
"#,
        )
        .unwrap();
        assert_eq!(f.hosts.len(), 2);
        assert_eq!(f.hosts["ts-ubuntu"].target.as_deref(), Some("ts-ubuntu"));
    }
}
