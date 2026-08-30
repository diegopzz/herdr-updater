# herdr-updater

An original Herdr plugin and standalone Rust CLI by **diegopzz**. It keeps
Herdr core and installed Herdr plugins current without silently overwriting
local forks, crossing protocol boundaries, or turning a failed network check
into a false green result.

The default policy is `notify`: every command may inspect, but nothing updates
until you explicitly opt into `policy = "auto"`.

## What it covers

- Herdr core version, protocol, server state, and live-handoff capability.
- Every plugin reported by `herdr plugin list --json`.
- GitHub branch/default-ref updates, classified with `gh` first and the public
  GitHub API only as a fallback.
- Immutable tag and commit pins.
- Linked plugin checkouts and forks, which are reported but never reinstalled.
- Cross-host core and plugin drift using SSH aliases from `hosts.toml`.
- Searchable plugin store backed by Herdr's official marketplace snapshot.
- Exact-commit plugin synchronization across connected Herdr computers.
- Custom check, catalog refresh, fleet sync, startup delay, jitter, and quiet-hour schedules.
- Append-only update history plus plugin rollback and resume.
- Linux, macOS, and Windows release binaries with SHA-256 verification.

## Safety model

`herdr-updater` applies only an installed GitHub plugin revision that is an
ancestor of its upstream branch revision. Ahead, diverged, pinned, linked,
unmanaged, rate-limited, unreachable, and otherwise unknown states are held.

Herdr core updates have two additional gates:

1. A running server must advertise live handoff.
2. A protocol change is held unless it is explicitly allowed as part of a
   staged rollout (`allow_protocol_change = true` or
   `--allow-protocol-change`).

After a core update, the CLI verifies the client/server version and refreshes
outdated built-in agent integrations. After a plugin reinstall, it re-reads the
plugin registry, verifies the resolved commit, and exercises action discovery.

Every subprocess has a hard deadline and receives an argv array. User-derived
owners, repositories, refs, subdirectories, and SSH aliases are validated
before they become arguments or URLs.

## Install

```sh
herdr plugin install diegopzz/herdr-updater
```

No Rust toolchain is required for a release install. The plugin launcher
downloads the matching GitHub release binary on first use and verifies it
against `checksums-<version>.txt`. The install step also places the standalone
`herdr-updater` binary in `~/.local/bin` on Unix or WindowsApps on Windows.

For local development:

```sh
cargo build --release
herdr plugin link /path/to/herdr-updater
herdr plugin action list --plugin herdr-updater
```

## Commands

| Command | Behavior |
| --- | --- |
| `check` | Inspect core and plugins; never mutate. |
| `plan` | Show `CURRENT`, `UPDATE`, `HOLD`, or `ERROR` for each target. |
| `apply` / `update` | Execute `UPDATE` decisions only when policy is `auto`. |
| `fleet` | Report core version/protocol and plugin inventory drift over SSH. |
| `search [query]` | Search Herdr's marketplace by id, name, source, language, or description. |
| `install <id>` | Preview and install a marketplace plugin at its indexed commit. |
| `store` | Open the keyboard-driven store UI in the current pane. |
| `open-store` | Open the store as a focused popup from inside Herdr. |
| `sync export` | Save this computer's managed plugin commits as reviewed desired state. |
| `sync plan` | Compare connected computers without changing them. |
| `sync apply --yes` | Reconcile safe differences and verify every remote result. |
| `schedule install` | Install the current user's background schedule. |
| `schedule status` | Show the scheduler resource and next internal check. |
| `schedule remove` | Remove only the scheduler resources owned by this plugin. |
| `history` | Read the append-only `state.jsonl` audit log. |
| `rollback` | Reinstall a plugin at its pre-update commit and pin it there. |
| `resume` | Return a rolled-back plugin to its recorded branch/default ref. |
| `startup` | Check on Herdr startup; in `auto`, update plugins but never core. |

Common options:

```text
--json
--timeout <seconds>
--config <path>
--only <plugin-id>
--hosts <alias,alias>
--core-only
--plugins-only
--allow-protocol-change
--sort <relevance|stars|trending|recent|name>
--limit <count>
--refresh
-y, --yes
```

Exit codes are stable for scripts:

| Code | Meaning |
| --- | --- |
| `0` | No action needed, or requested mutation succeeded. |
| `1` | An update needs attention, nothing can roll back/resume, or apply failed. |
| `2` | A check is unknown or config/state is invalid. |
| `3` | Command-line usage error. |

## Configuration

Copy [`config.example.toml`](config.example.toml) to the directory printed by:

```sh
herdr plugin config-dir herdr-updater
```

Minimal opt-in configuration:

```toml
policy = "auto"
trusted_owners = ["diegopzz"]
allow = ["diegopzz/herdr-*"]
```

An empty allowlist means all valid GitHub plugin sources are eligible, while
`trusted_owners` narrows that set by owner. Tag/commit pins remain immutable.
Fast-forward-only enforcement cannot be disabled.

Every timing value is customizable with `s`, `m`, `h`, and `d` units, including
compound values such as `1h30m`:

```toml
check_interval = "2h"
catalog_refresh_interval = "20m"
fleet_sync_interval = "8h"
initial_delay = "3m"
jitter = "10m"
quiet_hours = "22:30-07:00"

# Both are opt-in. `policy = "auto"` is also required for unattended writes.
scheduled_fleet_sync = true
sync_update_settings = false
```

`schedule install` creates a user-scoped systemd timer on Linux, launchd agent
on macOS, or Task Scheduler task on Windows. The internal state prevents double
runs, applies the initial delay on every platform, honors quiet hours, adds
bounded jitter, and uses capped retry backoff. A bounded scheduler heartbeat
observes the state deadline without stretching a jittered interval toward the
next full interval.
Installing or removing a schedule is always explicit; installing the plugin
does not silently create an operating-system task. Rerun `schedule install`
after changing timing settings so the native scheduler receives the new
heartbeat.

## Plugin store inside Herdr

The store consumes Herdr's [official marketplace snapshot](https://assets.herdr.dev/plugins/index.json),
caches it with a configurable TTL, and falls back to a clearly labeled stale
cache if the network is down. Open it from Herdr's action picker with
`herdr-updater.open-store`, or run:

```sh
herdr plugin action invoke herdr-updater.open-store
herdr-updater search "file viewer" --sort stars --limit 20
herdr-updater install <plugin-id>
```

Inside the popup, `/` searches, arrows or `j`/`k` navigate, `r` refreshes,
Enter previews installation, and `q` closes it. Installs use the exact commit
published in the snapshot and Herdr's native install flow. Marketplace entries
are discovery metadata, not a security review: inspect unfamiliar plugin code
and the native Herdr install preview before confirming it.

## Fleet inventory

`fleet` reads the first existing file:

1. `~/.config/herdr-updater/hosts.toml`
2. `~/.config/herdr-mirror/hosts.toml`

The shared shape is intentionally small:

```toml
[hosts.laptop]
target = "laptop"

[hosts.server]
target = "server"
```

Targets are SSH aliases, so ProxyJump/ProxyCommand and keys stay in
`~/.ssh/config`; this repository contains no credentials. Fleet mode is
read-only. A host that cannot return either core status or plugin inventory is
unknown and excluded from agreement claims.

## Fleet synchronization

Synchronization uses the same SSH aliases and follows an export-review-plan-
apply flow:

```sh
herdr-updater sync export
herdr-updater sync plan
herdr-updater sync apply --yes
```

`desired.toml` records exact resolved commits and enabled state for managed
GitHub plugins. Linked plugins, local forks, different sources, remote-only
plugins, protocol splits, unreachable hosts, and incomplete metadata are held,
never overwritten or uninstalled. Apply re-probes each changed host and only
reports success when source, commit, and enabled state match. Set
`sync_update_settings = true` only when every target should share this
plugin's non-secret update policy; machine-local SSH configuration is never
copied.

For unattended fleet reconciliation, enable both `policy = "auto"` and
`scheduled_fleet_sync = true`, then install the schedule. Notify policy still
produces plans but performs no background writes.

## Roadmap status

- [x] Safe Herdr core and plugin update planning and application.
- [x] Fast-forward checks, protected linked/forked plugins, rollback, and resume.
- [x] Fleet drift inventory with protocol-aware holds.
- [x] Searchable in-Herdr plugin store with native installation.
- [x] Reviewed exact-commit synchronization across connected computers.
- [x] Optional update-policy synchronization across computers.
- [x] Fully configurable intervals, quiet hours, jitter, and retry backoff.
- [x] User-scoped Linux, macOS, and Windows schedulers.
- [x] Cross-platform CI and checksum-verified release artifacts.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

The crate deliberately keeps a small dependency surface: `crossterm`, `serde`,
`serde_json`, and `toml`, all pinned by `Cargo.lock`.

## License

MIT © 2026 diegopzz
