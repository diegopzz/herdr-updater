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

After the first release:

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

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

The crate deliberately keeps a small dependency surface: `serde`,
`serde_json`, and `toml`, all pinned by `Cargo.lock`.

## License

MIT © 2026 diegopzz
