#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

const CURRENT_STATUS: &str = r#"{"client":{"version":"0.8.2","channel":"stable","protocol":20,"binary":"/fake/herdr"},"server":{"status":"running","running":true,"version":"0.8.2","protocol":20,"capabilities":{"live_handoff":true,"detached_server_daemon":true},"compatible":true,"restart_needed":false}}"#;

fn run(args: &[&str], status: &str, latest: &str) -> Output {
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

    let path = format!("{}:/usr/bin:/bin", bin.display());
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-updater"))
        .args(args)
        .env("HERDR_BIN_PATH", &herdr)
        .env("TEST_STATUS", status)
        .env("TEST_LATEST", latest)
        .env("TEST_CONFIG_DIR", &config)
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
