#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

const CURRENT_STATUS: &str = r#"{"client":{"version":"0.8.2","channel":"stable","protocol":20,"binary":"/fake/herdr"},"server":{"status":"running","running":true,"version":"0.8.2","protocol":20,"capabilities":{"live_handoff":true,"detached_server_daemon":true},"compatible":true,"restart_needed":false}}"#;

fn run(args: &[&str], status: &str, latest: &str) -> Output {
    run_seeded(args, status, latest, |_| {})
}

/// Same fixture, with a chance to write files into the config directory first
/// — history and config-parsing behaviour is only observable that way.
fn run_seeded(
    args: &[&str],
    status: &str,
    latest: &str,
    seed: impl FnOnce(&std::path::Path),
) -> Output {
    let root = std::env::temp_dir().join(format!(
        "herdr-updater-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let bin = root.join("bin");
    let config = root.join("config");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&config).unwrap();

    let herdr = bin.join("herdr-fixture");
    std::fs::write(
        &herdr,
        r#"#!/bin/sh
if [ "$1" = "status" ]; then
  printf '%s\n' "$TEST_STATUS"
elif [ "$1" = "plugin" ] && [ "$2" = "config-dir" ]; then
  printf '%s\n' "$TEST_CONFIG_DIR"
elif [ "$1" = "plugin" ] && [ "$2" = "list" ]; then
  printf '%s\n' '{"result":{"plugins":[]}}'
elif [ "$1" = "plugin" ] && [ "$2" = "action" ]; then
  printf '%s\n' '{"result":{"actions":[]}}'
else
  exit 0
fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o755)).unwrap();

    let curl = bin.join("curl");
    std::fs::write(&curl, "#!/bin/sh\nprintf '%s\\n' \"$TEST_LATEST\"\n").unwrap();
    std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();

    seed(&config);

    let path = format!("{}:/usr/bin:/bin", bin.display());
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-updater"))
        .args(args)
        .env("HERDR_BIN_PATH", &herdr)
        .env("TEST_STATUS", status)
        .env("TEST_LATEST", latest)
        .env("TEST_CONFIG_DIR", &config)
        .env("HOME", &root)
        .env("PATH", path)
        .output()
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
    output
}

#[test]
fn current_check_exits_zero() {
    let output = run(
        &["check", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["core"]["action"]["action"], "current");
}

#[test]
fn available_protocol_change_exits_one_and_is_held() {
    let output = run(
        &["plan", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.9.0","protocol":21}"#,
    );
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["core"]["action"]["action"], "hold");
}

#[test]
fn malformed_status_is_unknown_not_current() {
    let output = run(
        &["check", "--json"],
        "not-json",
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn bad_command_is_usage_error() {
    let output = run(
        &["bogus"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn schedule_status_is_read_only_and_succeeds_without_an_installed_timer() {
    let output = run(
        &["schedule", "status", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["installed"], false);
}

#[test]
fn sync_plan_without_connected_hosts_is_incomplete_not_green() {
    let output = run(
        &["sync", "plan", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("no fleet hosts")))));
}

#[test]
fn sync_plan_reports_an_unmatched_host_selection() {
    let output = run(
        &["sync", "plan", "--hosts", "missing-host", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("missing-host")))));
}

#[test]
fn sync_plan_with_unknown_local_protocol_fails_closed() {
    let output = run(
        &["sync", "plan", "--json"],
        "not-json",
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let decisions = json["decisions"].as_array().expect("sync decisions");
    assert!(decisions
        .iter()
        .any(|decision| decision["action"]["action"] == "offline"));
    assert!(!decisions.iter().any(|decision| matches!(
        decision["action"]["action"].as_str(),
        Some("install" | "update" | "enable" | "disable" | "sync_settings")
    )));
}

#[test]
fn invalid_marketplace_sort_is_a_usage_error() {
    let output = run(
        &["search", "viewer", "--sort", "mystery"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn a_misspelled_config_key_fails_closed_instead_of_being_ignored() {
    // `trusted_owner` parses clean under serde defaults and silently means no
    // owner restriction at all, which is the opposite of what was written.
    let output = run_seeded(
        &["check", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
        |config| {
            std::fs::write(
                config.join("config.toml"),
                "policy = \"auto\"\ntrusted_owner = [\"diegopzz\"]\n",
            )
            .unwrap();
        },
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("trusted_owners"), "{stderr}");
}

#[test]
fn an_unrecognised_config_key_warns_in_the_report_without_failing() {
    let output = run_seeded(
        &["check", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
        |config| {
            std::fs::write(config.join("config.toml"), "future_setting = 3\n").unwrap();
        },
    );
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("future_setting")))));
}

#[test]
fn a_core_version_ahead_of_the_manifest_is_held_not_applied() {
    // policy = auto is the dangerous case: string inequality would have made
    // this an UPDATE, and an UPDATE here is a downgrade.
    let output = run_seeded(
        &["plan", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.1","protocol":20}"#,
        |config| {
            std::fs::write(config.join("config.toml"), "policy = \"auto\"\n").unwrap();
        },
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["core"]["state"]["relation"], "ahead");
    assert_eq!(json["core"]["state"]["update_available"], false);
    assert_eq!(json["core"]["action"]["action"], "hold");
    assert!(json["core"]["action"]["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("downgrade")));
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn an_unparseable_core_version_is_an_error_not_a_green_check() {
    let status = CURRENT_STATUS.replace(r#""version":"0.8.2""#, r#""version":"nightly""#);
    let output = run(
        &["check", "--json"],
        &status,
        r#"{"version":"0.9.0","protocol":20}"#,
    );
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["core"]["state"]["relation"], "unknown");
    assert_eq!(json["core"]["action"]["action"], "error");
}

#[test]
fn history_filters_compose_and_limit_keeps_the_most_recent() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let output = run_seeded(
        &["history", "--json", "--limit", "2"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
        |config| {
            let lines: Vec<String> = (0..5)
                .map(|index| {
                    format!(
                        r#"{{"unix_seconds":{},"kind":"plugin_updated","target":"p{index}","previous":"a","current":"b"}}"#,
                        now - 100 + index
                    )
                })
                .collect();
            std::fs::write(config.join("state.jsonl"), lines.join("\n") + "\n").unwrap();
        },
    );
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let events = json.as_array().expect("history array");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["target"], "p3");
    assert_eq!(events[1]["target"], "p4");
}

#[test]
fn history_since_excludes_older_entries() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let output = run_seeded(
        &["history", "--json", "--since", "1h"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
        |config| {
            let old = now - 7 * 24 * 60 * 60;
            let recent = now - 60;
            std::fs::write(
                config.join("state.jsonl"),
                format!(
                    "{{\"unix_seconds\":{old},\"kind\":\"plugin_updated\",\"target\":\"old\",\"previous\":\"a\",\"current\":\"b\"}}\n                     {{\"unix_seconds\":{recent},\"kind\":\"plugin_updated\",\"target\":\"new\",\"previous\":\"a\",\"current\":\"b\"}}\n"
                ),
            )
            .unwrap();
        },
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let events = json.as_array().expect("history array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["target"], "new");
}

#[test]
fn doctor_reports_every_area_and_never_panics_on_a_broken_config() {
    let output = run_seeded(
        &["doctor", "--json"],
        CURRENT_STATUS,
        r#"{"version":"0.8.2","protocol":20}"#,
        |config| {
            std::fs::write(config.join("config.toml"), "trusted_owner = [\"x\"]\n").unwrap();
        },
    );
    // A config that will not load is exactly why somebody runs doctor, so it
    // must report and continue rather than abort at the first check.
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let checks = json["checks"].as_array().expect("doctor checks");
    assert!(json["failures"].as_u64().unwrap() >= 1);
    for area in ["config", "herdr", "tools", "state", "fleet"] {
        assert!(
            checks.iter().any(|check| check["area"] == area),
            "doctor did not report the {area} area"
        );
    }
    assert!(checks
        .iter()
        .any(|check| check["level"] == "fail" && check["area"] == "config"));
}

/// The version is written in seven places and the launcher downloads
/// `v$VERSION` release assets by name, so a file left behind does not fail at
/// build time — it fails on somebody else's machine, at install time, with a
/// 404 for an asset that was never published.
#[test]
fn every_file_that_carries_the_version_agrees_with_the_manifest() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let version = env!("CARGO_PKG_VERSION");
    for (file, needle) in [
        ("herdr-plugin.toml", format!("version = \"{version}\"")),
        ("bin/herdr-updater", format!("VERSION=\"{version}\"")),
        ("bin/herdr-updater.ps1", format!("$Version = \"{version}\"")),
        ("scripts/install-cli.sh", format!("/{version}/")),
        ("scripts/install-cli.ps1", format!("\\{version}\\")),
    ] {
        let contents = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(
            contents.contains(&needle),
            "{file} does not carry version {version} (looked for {needle:?})"
        );
    }
}

fn write_executable(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn release_launcher_falls_back_to_https_when_gh_is_unauthenticated() {
    let root = std::env::temp_dir().join(format!(
        "herdr-updater-launcher-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let plugin_bin = root.join("plugin/bin");
    let tools = root.join("tools");
    std::fs::create_dir_all(&plugin_bin).unwrap();
    std::fs::create_dir_all(&tools).unwrap();

    let launcher = plugin_bin.join("herdr-updater");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bin/herdr-updater"),
        &launcher,
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();

    write_executable(
        &tools.join("gh"),
        "#!/bin/sh\nprintf attempted > \"$TEST_GH_MARKER\"\nexit 1\n",
    );
    write_executable(
        &tools.join("uname"),
        "#!/bin/sh\ncase \"$1\" in -s) printf 'Linux\\n' ;; -m) printf 'x86_64\\n' ;; esac\n",
    );
    write_executable(
        &tools.join("curl"),
        r#"#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    *) shift ;;
  esac
done
case "$out" in
  *checksums*) printf 'fixturehash  %s\n' "$TEST_ASSET" > "$out" ;;
  *) printf 'fixture archive\n' > "$out" ;;
esac
printf used >> "$TEST_CURL_MARKER"
"#,
    );
    write_executable(
        &tools.join("sha256sum"),
        "#!/bin/sh\nprintf 'fixturehash  %s\\n' \"$1\"\n",
    );
    write_executable(
        &tools.join("tar"),
        r#"#!/bin/sh
destination=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -C) destination=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '#!/bin/sh\nprintf "%%s\\n" "$TEST_VERSION"\n' > "$destination/herdr-updater"
chmod 755 "$destination/herdr-updater"
"#,
    );

    let version = env!("CARGO_PKG_VERSION");
    let asset = format!("herdr-updater-{version}-x86_64-unknown-linux-gnu.tar.gz");
    let gh_marker = root.join("gh-attempted");
    let curl_marker = root.join("curl-used");
    let output = Command::new("sh")
        .arg(&launcher)
        .arg("version")
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .env("TEST_ASSET", &asset)
        .env("TEST_VERSION", version)
        .env("TEST_GH_MARKER", &gh_marker)
        .env("TEST_CURL_MARKER", &curl_marker)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), version);
    assert!(gh_marker.is_file());
    assert!(curl_marker.is_file());
    assert!(plugin_bin
        .join(format!(".cache/{version}/herdr-updater"))
        .is_file());
    std::fs::remove_dir_all(root).unwrap();
}
