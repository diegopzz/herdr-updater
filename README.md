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

Herdr core is held under the same rule, not a looser one. Installed and
published versions are **ordered**, never compared for inequality, so only a
version that is genuinely newer is an update:

| Relation | Meaning | Action |
| --- | --- | --- |
| `same` | Identical versions. | Current. |
| `behind` | The manifest is newer. | The only relation that may be applied. |
| `ahead` | This host is newer than the manifest. | Held — applying it would be a downgrade. |
| `unknown` | A version this build cannot parse. | **Error**, not "current". |

Versions follow semver precedence, so `0.10.0` is newer than `0.9.0` and
`0.9.0-rc.1` is older than `0.9.0`. An unorderable pair exits `2` rather than
reporting green, because a host running something we cannot read must never
look like a host that is current.

Three further gates apply to core:

1. A running server must advertise live handoff.
2. A protocol change is held unless it is explicitly allowed as part of a
   staged rollout (`allow_protocol_change = true` or
   `--allow-protocol-change`).
3. The client must track the channel `latest.json` describes (`stable`).
   A client on any other channel is being compared against a manifest that does
   not describe it, which is held unless `allow_channel_mismatch = true`.

After a core update, the CLI verifies the client/server version and refreshes
outdated built-in agent integrations. After a plugin reinstall, it re-reads the
plugin registry, verifies the resolved commit, and exercises action discovery.

GitHub comparisons prefer an authenticated `gh` and fall back to the public
API, which allows 60 requests an hour for the whole machine — one per
GitHub-sourced plugin per inspection. Exhausting it is reported as
`rate_limited` and exits `2`: a hold would read as green, and "we could not
check" must never mean "current". `herdr-updater doctor` prints the remaining
budget and when it resets.

Every subprocess has a hard deadline and receives an argv array. User-derived
owners, repositories, refs, subdirectories, and SSH aliases are validated
before they become arguments or URLs.

## Install

```sh
herdr plugin install diegopzz/herdr-updater
```

No Rust toolchain is required for a release install. Prebuilt binaries cover
x86-64 and ARM64 Linux, Intel and Apple-silicon macOS, and x86-64 Windows. The
plugin launcher downloads the matching GitHub release binary on first use and
verifies it against `checksums-<version>.txt`.

The checksum proves the archive arrived intact; provenance proves where it came
from. Every release artifact is attested to the workflow, repository, and commit
that produced it:

```sh
gh attestation verify herdr-updater-<version>-<target>.tar.gz \
  --repo diegopzz/herdr-updater
```
 The install step also places the standalone
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
| `doctor` | Check this tool's own environment and report what is degraded. |
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
--since <duration>
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

## Diagnosing the updater itself

Every other command reports on Herdr. `doctor` reports on this tool, because
its characteristic failure is silence — a schedule that was never installed, a
`curl` missing from `PATH`, a config directory that is not writable, a GitHub
budget that ran out three plugins ago. Each produces a tool that runs, exits,
and changes nothing, and the exit code alone never says which one fired.

```sh
herdr-updater doctor
herdr-updater doctor --json
```

It checks the config file and any keys this build does not understand, the
`herdr` binary and its status, the release manifest and the resulting core
verdict, the five external tools and the capability each one gates, the GitHub
API budget with its reset time, whether the state directory is writable,
history integrity, the schedule and whether it is actually firing, and the
fleet hosts file.

| Level | Meaning | Exit |
| --- | --- | --- |
| `ok` | Checked, and working. | `0` |
| `warn` | A capability is off or degraded; something you asked for will not happen. | `1` |
| `fail` | A check this tool depends on cannot run, so its answers about that area are not answers. | `2` |

Everything that is not `ok` carries a remedy. `doctor` loads its own config
rather than inheriting one, so a config file that will not parse is reported as
a `fail` and the remaining checks still run — aborting there would answer the
question with the same silence being diagnosed.

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

Misspelled settings are refused rather than ignored. `trusted_owner` parses
cleanly under a defaulted config and silently means *no owner restriction at
all*, so any key within a small edit distance of a real one is an error naming
the key you meant. A key resembling nothing known only warns and is reported in
`check`, `plan`, and `doctor` output, because an older host mid-rollout must
not fail on a setting introduced by a newer build.

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
next full interval. The recurring native heartbeat is clamped to a one-minute
minimum, so sub-minute delay or jitter values have one-minute execution
resolution instead of creating a one-second background loop.
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

`fleet` reads the first of these that exists, and the report always names the
one it actually used:

1. `~/.config/herdr-updater/hosts.toml` — this plugin's own file.
2. `~/.config/herdr-mirror/hosts.toml` — the file belonging to
   [`herdr-mirror`](https://github.com/diegopzz/herdr-mirror), a **separate
   Herdr plugin** that mirrors remote Herdr servers into your local sidebar and
   drives them over SSH.

The second path is a deliberate fallback, not a copy and not a dependency.
`herdr-mirror` does a different job, but it needs the same answer to the same
question — *which machines are mine, and what is each one called over SSH?* —
and anyone running both has already written that list once. Reading it means a
host added for mirroring is automatically a host this tool keeps updated, with
no second list to forget. Neither plugin requires the other: if you do not run
`herdr-mirror`, create the first file and the fallback never applies. If you do
run both, create the first file only when the two need different host sets,
because it wins outright rather than merging.

The two plugins are affected differently by a protocol split, which is why
`fleet` says so explicitly rather than warning about "the fleet" in general:
`herdr --remote` negotiates the protocol, so a local client cannot attach
across a split, while `herdr-mirror` runs the *remote* host's own herdr binary,
so both ends of that conversation already agree.

The shared shape is intentionally small, and is the same file `herdr-mirror`
reads:

```toml
[hosts.laptop]
target = "laptop"

[hosts.server]
target = "server"
```

Targets are SSH aliases, so ProxyJump/ProxyCommand and keys stay in
`~/.ssh/config`; this repository contains no credentials. `[hosts.laptop]` with
no `target` means `ssh laptop`, matching `herdr-mirror`. Fleet mode is
read-only, and fans out over at most `max_concurrency` hosts at a time. A host
that cannot return either core status or plugin inventory is unknown and
excluded from agreement claims.

## Fleet synchronization

Synchronization uses the same SSH aliases and follows an export-review-plan-
apply flow:

```sh
herdr-updater sync export
herdr-updater sync plan
herdr-updater sync apply --yes
```

`desired.toml` records exact resolved commits, enabled state, and each managed
plugin's minimum Herdr version. Linked plugins, local forks, different sources,
remote-only plugins, incompatible Herdr versions, protocol splits, unreachable
hosts, and incomplete metadata are held, never overwritten or uninstalled.
Apply re-probes each changed host and only reports success when source, commit,
and enabled state match. Desired files created before compatibility metadata was
added remain readable but hold changes until `sync export` refreshes them. Set
`sync_update_settings = true` only when every target should share this
plugin's non-secret update policy; machine-local SSH configuration is never
copied.

For unattended fleet reconciliation, enable both `policy = "auto"` and
`scheduled_fleet_sync = true`, then install the schedule. Notify policy still
produces plans but performs no background writes.

Timestamps in `history` and `schedule status` are UTC, so the same log reads
the same way on every host in a fleet. `history` accepts `--limit`, `--since`,
and `--only`, applied in that order, so `--limit` always means the most recent
entries rather than the first ones in the file:

```sh
herdr-updater history --since 7d
herdr-updater history --only herdr-mirror --limit 5
```

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
- [x] ARM64 Linux release binaries alongside x86-64 and both macOS targets.
- [x] Signed build provenance for every release artifact.
- [x] Version-ordered core updates that refuse to downgrade.
- [x] `doctor` preflight for the updater's own environment.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

The crate deliberately keeps a small dependency surface: `crossterm`, `serde`,
`serde_json`, and `toml`, all pinned by `Cargo.lock`. Advisories against that
tree are checked weekly by `cargo audit` rather than on every pull request, so
an advisory against a transitive dependency does not turn an unrelated review
red.

The minimum supported Rust version is declared in `Cargo.toml` and verified by
CI against that exact toolchain — it is a tested claim, not a decoration.

## License

MIT © 2026 diegopzz
