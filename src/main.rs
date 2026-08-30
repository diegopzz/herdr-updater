//! Original Herdr core-and-plugin updater by diegopzz.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

mod config;
mod exec;
mod fleet;
mod herdr;
mod history;
mod plugins;

const USAGE: &str = "\
herdr-updater -- keep Herdr core and plugins current without overwriting forks

USAGE
    herdr-updater <COMMAND> [OPTIONS]

COMMANDS
    check       inspect core and plugins; never mutate
    plan        show the policy decision for every target; never mutate
    apply       execute safe UPDATE decisions when policy = \"auto\"
    update      alias for apply
    fleet       report Herdr and plugin drift across configured SSH hosts
    history     print the append-only update/rollback audit log
    rollback    pin plugin(s) to the revision before their latest update
    resume      move rolled-back plugin(s) back to their recorded tracking ref
    startup     startup hook; check by default, auto-update plugins only if opted in
    version     print this tool's version

OPTIONS
    --json                      machine-readable output
    --timeout <seconds>         per-command wall-clock deadline (default 20)
    --config <path>             override config.toml
    --only <plugin-id>          restrict plugin operations to one installed plugin
    --hosts <a,b>               fleet only: restrict to SSH aliases
    --core-only                 skip plugin inspection
    --plugins-only              skip Herdr core inspection
    --allow-protocol-change     explicitly allow a local core protocol change
    -h, --help                  show this text

EXIT CODES
    0 no action needed / success
    1 updates need attention, nothing to roll back, or an apply failed
    2 a check is unknown or configuration/state is invalid
    3 command-line usage error
";

#[derive(Debug)]
struct Args {
    command: String,
    json: bool,
    timeout: Duration,
    hosts: Vec<String>,
    only: Option<String>,
    config: Option<PathBuf>,
    core_only: bool,
    plugins_only: bool,
    allow_protocol_change: bool,
}

fn parse() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() || raw.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Err(String::new());
    }
    let mut args = Args {
        command: raw[0].clone(),
        json: false,
        timeout: Duration::from_secs(20),
        hosts: Vec::new(),
        only: None,
        config: None,
        core_only: false,
        plugins_only: false,
        allow_protocol_change: false,
    };
    let mut iter = raw.into_iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => args.json = true,
            "--timeout" => {
                let value = iter.next().ok_or("--timeout needs a value")?;
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| format!("--timeout: {value:?} is not a number"))?;
                if seconds == 0 || seconds > 3600 {
                    return Err("--timeout must be between 1 and 3600 seconds".into());
                }
                args.timeout = Duration::from_secs(seconds);
            }
            "--hosts" => {
                let value = iter.next().ok_or("--hosts needs a value")?;
                args.hosts = value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "--only" => args.only = Some(iter.next().ok_or("--only needs a plugin id")?),
            "--config" => {
                args.config = Some(PathBuf::from(iter.next().ok_or("--config needs a path")?));
            }
            "--core-only" => args.core_only = true,
            "--plugins-only" => args.plugins_only = true,
            "--allow-protocol-change" => args.allow_protocol_change = true,
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if args.core_only && args.plugins_only {
        return Err("--core-only and --plugins-only cannot be combined".into());
    }
    if args.only.is_some() && args.core_only {
        return Err("--only selects a plugin and cannot be combined with --core-only".into());
    }
    Ok(args)
}

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "herdr".to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "action", content = "reason")]
enum Action {
    Current,
    Update,
    Hold(String),
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
struct CoreReport {
    state: herdr::CoreState,
    action: Action,
}

#[derive(Debug, Clone, Serialize)]
struct PluginReport {
    #[serde(flatten)]
    state: plugins::PluginState,
    action: Action,
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    policy: config::Policy,
    config_path: String,
    config_exists: bool,
    core: Option<CoreReport>,
    plugins: Vec<PluginReport>,
    errors: Vec<String>,
}

fn core_action(state: &herdr::CoreState, cfg: &config::Config) -> Action {
    match herdr::gate(state) {
        Ok(()) if state.protocol_change && !cfg.allow_protocol_change => {
            Action::Hold("protocol change requires an explicit staged rollout".into())
        }
        Ok(()) if cfg.policy == config::Policy::Notify => Action::Hold("notify policy".into()),
        Ok(()) => Action::Update,
        Err(herdr::Hold::UpToDate) => Action::Current,
        Err(herdr::Hold::NoLiveHandoff) => {
            Action::Hold("running server cannot live-handoff".into())
        }
        Err(herdr::Hold::Unknown(error)) => Action::Error(error),
    }
}

fn plugin_action(state: &plugins::PluginState, cfg: &config::Config) -> Action {
    match plugins::decide(state, cfg) {
        plugins::Decision::Current => Action::Current,
        plugins::Decision::Update if cfg.policy == config::Policy::Notify => {
            Action::Hold("notify policy".into())
        }
        plugins::Decision::Update => Action::Update,
        plugins::Decision::Hold(reason) => Action::Hold(reason),
        plugins::Decision::Error(reason) => Action::Error(reason),
    }
}

fn inspect(args: &Args, loaded: &config::Loaded) -> Report {
    let bin = herdr_bin();
    let inspect_core = loaded.value.check_core && !args.plugins_only && args.only.is_none();
    let inspect_plugins = loaded.value.check_plugins && !args.core_only;
    let core = inspect_core.then(|| {
        let state = herdr::inspect(&bin, args.timeout);
        let action = core_action(&state, &loaded.value);
        CoreReport { state, action }
    });
    let mut errors = Vec::new();
    let plugins = if inspect_plugins {
        match plugins::inspect_all(&bin, &loaded.value, args.only.as_deref(), args.timeout) {
            Ok(states) => states
                .into_iter()
                .map(|state| {
                    let action = plugin_action(&state, &loaded.value);
                    PluginReport { state, action }
                })
                .collect(),
            Err(error) => {
                errors.push(error);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    Report {
        policy: loaded.value.policy,
        config_path: loaded.path.display().to_string(),
        config_exists: loaded.existed,
        core,
        plugins,
        errors,
    }
}

fn has_errors(report: &Report) -> bool {
    !report.errors.is_empty()
        || report
            .core
            .as_ref()
            .is_some_and(|core| matches!(core.action, Action::Error(_)))
        || report
            .plugins
            .iter()
            .any(|plugin| matches!(plugin.action, Action::Error(_)))
}

fn needs_attention(report: &Report) -> bool {
    report
        .core
        .as_ref()
        .is_some_and(|core| core.state.update_available)
        || report
            .plugins
            .iter()
            .any(|plugin| plugin.state.update_available)
}

fn action_text(action: &Action) -> String {
    match action {
        Action::Current => "CURRENT".into(),
        Action::Update => "UPDATE".into(),
        Action::Hold(reason) => format!("HOLD -- {reason}"),
        Action::Error(reason) => format!("ERROR -- {reason}"),
    }
}

fn print_report(report: &Report, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!(
        "policy: {:?}  config: {}{}",
        report.policy,
        report.config_path,
        if report.config_exists {
            ""
        } else {
            " (defaults)"
        }
    );
    if let Some(core) = &report.core {
        println!("\nHerdr core");
        println!(
            "  installed  {} (protocol {})",
            if core.state.installed.is_empty() {
                "unknown"
            } else {
                &core.state.installed
            },
            core.state.protocol
        );
        println!(
            "  latest     {}{}",
            core.state.latest.as_deref().unwrap_or("unknown"),
            core.state
                .latest_protocol
                .map(|protocol| format!(" (protocol {protocol})"))
                .unwrap_or_default()
        );
        println!("  action     {}", action_text(&core.action));
    }
    if !report.plugins.is_empty() {
        println!("\nPlugins");
        for plugin in &report.plugins {
            let source = match (&plugin.state.owner, &plugin.state.repo) {
                (Some(owner), Some(repo)) => format!("{owner}/{repo}"),
                _ => plugin.state.source_kind.clone(),
            };
            println!(
                "  {:<24} {:<28} {}",
                plugin.state.plugin_id,
                source,
                action_text(&plugin.action)
            );
        }
    }
    for error in &report.errors {
        println!("\nERROR -- {error}");
    }
}

fn check_or_plan(args: &Args, loaded: &config::Loaded) -> i32 {
    let report = inspect(args, loaded);
    print_report(&report, args.json);
    if has_errors(&report) {
        2
    } else if needs_attention(&report) {
        1
    } else {
        0
    }
}

fn apply_report(args: &Args, loaded: &config::Loaded, startup: bool) -> i32 {
    let report = inspect(args, loaded);
    if has_errors(&report) {
        print_report(&report, args.json);
        return 2;
    }
    if loaded.value.policy == config::Policy::Notify {
        print_report(&report, args.json);
        if !args.json && needs_attention(&report) {
            println!("\nnotify policy: no changes were made");
        }
        return i32::from(needs_attention(&report));
    }

    let bin = herdr_bin();
    let history_path = history::path(
        loaded
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    );
    let mutation_timeout = args.timeout.max(Duration::from_secs(120));
    let mut failures = Vec::new();
    let mut applied = Vec::new();

    if !startup {
        if let Some(core) = &report.core {
            if matches!(core.action, Action::Update) {
                match herdr::apply(&bin, &core.state, mutation_timeout) {
                    Ok(result) => {
                        let event = history::Event::new(
                            history::EventKind::CoreUpdated,
                            "herdr",
                            core.state.installed.clone(),
                            result.installed.clone(),
                            None,
                        );
                        if let Err(error) = history::append(&history_path, &event) {
                            failures.push(error);
                        } else {
                            applied.push(format!("herdr -> {}", result.installed));
                        }
                    }
                    Err(error) => failures.push(format!("herdr: {error}")),
                }
            }
        }
    }

    let mut plugin_updates: Vec<&PluginReport> = report
        .plugins
        .iter()
        .filter(|plugin| matches!(plugin.action, Action::Update))
        .collect();
    plugin_updates.sort_by_key(|plugin| plugin.state.plugin_id == "herdr-updater");
    for plugin in plugin_updates {
        let Some(previous) = plugin.state.installed_sha.as_deref() else {
            failures.push(format!(
                "{}: installed revision is unknown",
                plugin.state.plugin_id
            ));
            continue;
        };
        let Some(expected) = plugin.state.remote_sha.as_deref() else {
            failures.push(format!(
                "{}: remote revision is unknown",
                plugin.state.plugin_id
            ));
            continue;
        };
        let reference = plugin.state.requested_ref.as_deref();
        let result =
            plugins::install(&bin, &plugin.state, reference, mutation_timeout).and_then(|_| {
                plugins::verify(&bin, &plugin.state.plugin_id, expected, mutation_timeout)
            });
        match result {
            Ok(()) => {
                let event = history::Event::new(
                    history::EventKind::PluginUpdated,
                    plugin.state.plugin_id.clone(),
                    previous,
                    expected,
                    plugin.state.requested_ref.clone(),
                );
                if let Err(error) = history::append(&history_path, &event) {
                    failures.push(error);
                } else {
                    applied.push(format!("{} -> {}", plugin.state.plugin_id, &expected[..12]));
                }
            }
            Err(error) => failures.push(format!("{}: {error}", plugin.state.plugin_id)),
        }
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "applied": applied,
                "failures": failures,
            }))
            .unwrap_or_else(|_| "{}".into())
        );
    } else {
        for item in &applied {
            println!("UPDATED {item}");
        }
        for error in &failures {
            println!("FAILED  {error}");
        }
        if applied.is_empty() && failures.is_empty() {
            println!("no changes needed");
        }
    }
    i32::from(!failures.is_empty())
}

fn print_history(args: &Args, loaded: &config::Loaded) -> i32 {
    let path = history::path(
        loaded
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    );
    match history::read(&path) {
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            2
        }
        Ok(events) if args.json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&events).unwrap_or_else(|_| "[]".into())
            );
            0
        }
        Ok(events) => {
            if events.is_empty() {
                println!("no update history in {}", path.display());
            }
            for event in events {
                println!(
                    "{}  {:?}  {}  {} -> {}",
                    event.unix_seconds, event.kind, event.target, event.previous, event.current
                );
            }
            0
        }
    }
}

fn rollback_or_resume(args: &Args, loaded: &config::Loaded, resume: bool) -> i32 {
    let bin = herdr_bin();
    let dir = loaded
        .path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let state_path = history::path(dir);
    let events = match history::read(&state_path) {
        Ok(events) => events,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    let latest = history::latest_plugins(&events);
    let mut candidates: Vec<_> = latest
        .values()
        .filter(|event| args.only.as_ref().map_or(true, |id| id == &event.target))
        .filter(|event| {
            if resume {
                event.kind == history::EventKind::PluginRolledBack
            } else {
                event.kind == history::EventKind::PluginUpdated
            }
        })
        .cloned()
        .collect();
    candidates.sort_by(|a, b| a.target.cmp(&b.target));
    if candidates.is_empty() {
        println!("nothing to {}", if resume { "resume" } else { "roll back" });
        return 1;
    }

    let mutation_timeout = args.timeout.max(Duration::from_secs(120));
    let mut failures = Vec::new();
    let mut applied = Vec::new();
    for event in candidates {
        let inspected =
            match plugins::inspect_all(&bin, &loaded.value, Some(&event.target), args.timeout) {
                Ok(mut states) if states.len() == 1 => states.remove(0),
                Ok(_) => {
                    failures.push(format!("{}: installed state is ambiguous", event.target));
                    continue;
                }
                Err(error) => {
                    failures.push(format!("{}: {error}", event.target));
                    continue;
                }
            };
        let (reference, kind, expected, tracking_ref) = if resume {
            (
                event.tracking_ref.as_deref(),
                history::EventKind::PluginResumed,
                None,
                event.tracking_ref.clone(),
            )
        } else {
            (
                Some(event.previous.as_str()),
                history::EventKind::PluginRolledBack,
                Some(event.previous.as_str()),
                event.tracking_ref.clone(),
            )
        };
        let result =
            plugins::install(&bin, &inspected, reference, mutation_timeout).and_then(|_| {
                if let Some(expected) = expected {
                    plugins::verify(&bin, &event.target, expected, mutation_timeout)?;
                    Ok(expected.to_string())
                } else {
                    let after = plugins::inspect_all(
                        &bin,
                        &loaded.value,
                        Some(&event.target),
                        args.timeout,
                    )?;
                    after
                        .first()
                        .and_then(|state| state.installed_sha.clone())
                        .ok_or_else(|| "resumed plugin has no resolved commit".to_string())
                }
            });
        match result {
            Ok(current) => {
                let previous = inspected
                    .installed_sha
                    .unwrap_or_else(|| event.current.clone());
                let next = history::Event::new(
                    kind,
                    event.target.clone(),
                    previous,
                    current,
                    tracking_ref,
                );
                if let Err(error) = history::append(&state_path, &next) {
                    failures.push(error);
                } else {
                    applied.push(event.target);
                }
            }
            Err(error) => failures.push(format!("{}: {error}", event.target)),
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "changed": applied,
                "failures": failures,
            }))
            .unwrap_or_else(|_| "{}".into())
        );
    } else {
        for target in &applied {
            println!(
                "{} {target}",
                if resume { "RESUMED" } else { "ROLLED BACK" }
            );
        }
        for error in &failures {
            println!("FAILED {error}");
        }
    }
    i32::from(!failures.is_empty())
}

fn run(args: &mut Args) -> i32 {
    if args.command == "version" {
        println!("herdr-updater {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    if args.command == "fleet" {
        return fleet::cmd_fleet(&args.hosts, args.timeout, args.json);
    }
    let bin = herdr_bin();
    let mut loaded = match config::load(args.config.as_deref(), &bin, args.timeout) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    if args.allow_protocol_change {
        loaded.value.allow_protocol_change = true;
    }
    match args.command.as_str() {
        "check" | "plan" => check_or_plan(args, &loaded),
        "apply" | "update" => apply_report(args, &loaded, false),
        "startup" if !loaded.value.startup_check => 0,
        "startup" => apply_report(args, &loaded, true),
        "history" => print_history(args, &loaded),
        "rollback" => rollback_or_resume(args, &loaded, false),
        "resume" => rollback_or_resume(args, &loaded, true),
        other => {
            eprintln!("herdr-updater: unknown command: {other}\n");
            eprint!("{USAGE}");
            3
        }
    }
}

fn main() {
    let mut args = match parse() {
        Ok(args) => args,
        Err(error) => {
            if !error.is_empty() {
                eprintln!("herdr-updater: {error}\n");
            }
            eprint!("{USAGE}");
            std::process::exit(3);
        }
    };
    std::process::exit(run(&mut args));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_changes_are_held_without_explicit_opt_in() {
        let state = herdr::CoreState {
            installed: "0.8.2".into(),
            channel: Some("stable".into()),
            binary: None,
            protocol: 20,
            server_status: Some("running".into()),
            server_running: true,
            server_version: Some("0.8.2".into()),
            server_protocol: Some(20),
            compatible: Some(true),
            live_handoff: true,
            detached_server_daemon: true,
            restart_needed: false,
            latest: Some("0.9.0".into()),
            latest_protocol: Some(21),
            update_available: true,
            protocol_change: true,
            error: None,
        };
        assert!(matches!(
            core_action(&state, &config::Config::default()),
            Action::Hold(reason) if reason.contains("staged rollout")
        ));
    }
}
