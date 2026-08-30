//! herdr-updater — keep herdr and its plugins current across a whole fleet.
//!
//! Exit codes are the contract scripts depend on, and they match the reference
//! project so the two are drop-in comparable:
//!
//!   0  nothing to do
//!   1  updates are available (check/plan) or an apply failed
//!   2  a check errored — the answer is unknown, not "up to date"
//!   3  usage error

use std::time::Duration;

mod exec;
mod fleet;
mod herdr;

const USAGE: &str = "\
herdr-updater — keep herdr and its plugins current across a fleet

USAGE
    herdr-updater <COMMAND> [OPTIONS]

COMMANDS
    check          herdr core status on this host vs herdr.dev/latest.json
    fleet          the same check across every configured host, as a drift report
    version        print this tool's version

OPTIONS
    --json         machine-readable output
    --timeout <s>  per-command wall-clock deadline (default 20)
    --hosts <a,b>  fleet only: restrict to these ssh aliases
    -h, --help     this text

EXIT CODES
    0 nothing to do   1 updates available   2 check errored   3 usage error
";

struct Args {
    cmd: String,
    json: bool,
    timeout: Duration,
    hosts: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() || raw.iter().any(|a| a == "-h" || a == "--help") {
        return Err(String::new()); // empty message = plain usage, exit 3
    }
    let mut args =
        Args { cmd: String::new(), json: false, timeout: Duration::from_secs(20), hosts: vec![] };
    let mut it = raw.into_iter();
    args.cmd = it.next().unwrap();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => args.json = true,
            "--timeout" => {
                let v = it.next().ok_or("--timeout needs a value")?;
                let secs: u64 = v.parse().map_err(|_| format!("--timeout: {v} is not a number"))?;
                if secs == 0 {
                    return Err("--timeout must be greater than 0".into());
                }
                args.timeout = Duration::from_secs(secs);
            }
            "--hosts" => {
                let v = it.next().ok_or("--hosts needs a value")?;
                args.hosts =
                    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(args)
}

/// herdr injects HERDR_BIN_PATH when it runs a plugin, so honour that first:
/// a plugin action must drive the *same* herdr that invoked it, not whatever
/// happens to be first on PATH.
fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("herdr-updater: {msg}\n");
            }
            eprint!("{USAGE}");
            std::process::exit(3);
        }
    };

    let code = match args.cmd.as_str() {
        "check" => cmd_check(&args),
        "fleet" => fleet::cmd_fleet(&args.hosts, args.timeout, args.json),
        "version" => {
            println!("herdr-updater {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("herdr-updater: unknown command: {other}\n");
            eprint!("{USAGE}");
            3
        }
    };
    std::process::exit(code);
}

fn cmd_check(args: &Args) -> i32 {
    let state = herdr::inspect(&herdr_bin(), args.timeout);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&state).unwrap_or_else(|_| "{}".into()));
    } else {
        println!("herdr core");
        if state.installed.is_empty() {
            println!("  installed : (unknown)");
        } else {
            println!("  installed : {} (protocol {})", state.installed, state.protocol);
        }
        match (&state.latest, state.latest_protocol) {
            (Some(v), Some(p)) => println!("  latest    : {v} (protocol {p})"),
            _ => println!("  latest    : (not checked)"),
        }
        println!(
            "  server    : {}{}",
            if state.server_running { "running" } else { "not running" },
            if state.server_running && !state.live_handoff {
                "  [no live handoff — an update would drop attached clients]"
            } else {
                ""
            }
        );
        match herdr::gate(&state) {
            Ok(()) => {
                println!(
                    "  action    : UPDATE {} -> {}",
                    state.installed,
                    state.latest.as_deref().unwrap_or("?")
                );
                if state.protocol_change {
                    println!(
                        "  ⚠ protocol {} -> {}: update every host together or mirrors and \
                         `herdr --remote` break between them",
                        state.protocol,
                        state.latest_protocol.unwrap_or(0)
                    );
                }
            }
            Err(herdr::Hold::UpToDate) => println!("  action    : up to date"),
            Err(herdr::Hold::NoLiveHandoff) => {
                println!("  action    : HOLD — running server cannot live-handoff")
            }
            Err(herdr::Hold::Unknown(e)) => println!("  action    : ERROR — {e}"),
        }
    }

    match herdr::gate(&state) {
        Ok(()) => 1,
        Err(herdr::Hold::UpToDate) => 0,
        Err(herdr::Hold::NoLiveHandoff) => 1,
        Err(herdr::Hold::Unknown(_)) => 2,
    }
}
