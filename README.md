# herdr-updater

Keep [herdr](https://herdr.dev) **and** its plugins current across a whole
fleet — not just the machine you happen to be sitting at.

> Status: **v0.1, early.** `check` and `fleet` are implemented and verified
> against a real four-host fleet. Plugin updates, `plan`/`apply` and rollback
> are next; see [Roadmap](#roadmap). This version cannot write anything.

## Why this exists

herdr carries a wire protocol version, and that version is what
`herdr --remote` negotiates on. Once you run herdr on more than one machine,
the failure that actually costs you a day is not "a plugin is out of date" —
it is **drift**: several hosts quietly running different versions, every one of
them individually healthy, every check green.

Here is that failure, real output, first run:

```
$ herdr-updater fleet
fleet (from /root/.config/herdr-mirror/hosts.toml)

  HOST             HERDR            PROTOCOL  SERVER
  this machine     0.8.2            20        running
  macbook          0.8.2            20        running
  portatil         0.7.5-preview…   18        running
  ts-ubuntu        0.8.2            20        running

  ⚠ PROTOCOL SPLIT — hosts are on different herdr protocols.
    `herdr --remote` negotiates on this, so a local client cannot attach to a
    remote server across the split.
    herdr-mirror is NOT affected the same way: it runs the remote host's own
    herdr binary, so both ends of that conversation already match.
    Update before relying on --remote, and close the drift regardless.
```

Four hosts, three agreeing, one silently two protocol versions behind. **Drift
is the product here**, not a side effect of an update run.

## Install

```bash
git clone https://github.com/diegopzz/herdr-updater
cd herdr-updater
cargo build --release
./target/release/herdr-updater fleet
```

Prebuilt release binaries land with v0.2, so hosts without a Rust toolchain can
run it — a real constraint, not a hypothetical: one macOS host in the test
fleet has no cargo at all.

## Commands

| Command | What it does |
|---|---|
| `check` | herdr core on this host vs `herdr.dev/latest.json` |
| `fleet` | the same check across every configured host, as a drift report |
| `version` | print the tool version |

Flags: `--json`, `--timeout <secs>` (default 20), `--hosts a,b` (fleet only).

### Exit codes

| Code | Meaning |
|---|---|
| `0` | nothing to do |
| `1` | updates available, or drift detected |
| `2` | a check **errored** — the answer is unknown, not "up to date" |
| `3` | usage error |

## Configuration

Fleet hosts are read from the first of these that exists:

1. `~/.config/herdr-updater/hosts.toml`
2. `~/.config/herdr-mirror/hosts.toml`

Reusing herdr-mirror's file is deliberate: if you already mirror a host, you
have already told the system it exists. The report always prints which file it
read — "which config won?" should never be a guess.

```toml
[hosts.macbook]
target = "macbook"      # ssh alias; defaults to the table key

[hosts.ts-ubuntu]
target = "ts-ubuntu"
```

Any other keys in that file (`always_control`, `poll_seconds`, …) are ignored,
not rejected.

## Design rules

These are load-bearing, not style preferences:

- **Every subprocess is argv, never a shell string.** Host aliases and plugin
  ids come from config files and from herdr's own output. Interpolating them
  into `sh -c` would make a repo named `; rm -rf ~` a code path. Aliases are
  additionally validated so a value can never grow into an ssh *option*.
- **Every subprocess has a hard wall-clock deadline.** This is meant to run at
  herdr *startup*; a wedged `ssh` against a dead network must not hang the
  terminal you are opening.
- **Unknown degrades to "unknown", never to "fine".** A host we could not read
  is excluded from the drift verdict and reported, not quietly counted as
  agreeing. A failed version check never becomes an update.
- **Warnings must not overstate blast radius.** An early draft of the protocol
  warning claimed a split stops mirrors working. Measurement disproved it: a
  protocol 18 host mirrors cleanly from a protocol 20 host, because
  herdr-mirror runs the remote host's *own* binary. A warning that overstates
  damage gets disabled, and then it protects nothing.
- **No unattended writes.** v0.1 cannot write at all. When apply lands, the
  default policy stays `notify`.

## Roadmap

- [x] herdr core version/protocol check, local and fleet-wide
- [x] Drift report with `--json` and scriptable exit codes
- [ ] Plugin registry check against upstream refs (`git ls-remote`), with
      branch/tag/commit channel classification
- [ ] `plan` / `apply`, fast-forward-only, `notify` by default
- [ ] `herdr update --handoff` orchestration, gated on the server's advertised
      `live_handoff` capability, **plus the mandatory post-update
      `herdr integration install <agent>`** — skipping that leaves agent hooks
      stale, which looks exactly like a broken integration
- [ ] Linked / forked plugin support: fetch, compare, and never reinstall over
      a local fork
- [ ] Streamer-safe binary replacement for herdr-mirror (pause, replace,
      restart) — replacing that binary under live streamers is a known breakage
- [ ] Post-update verification: exercise the plugin instead of trusting the
      installer's exit code
- [ ] Prebuilt release binaries with SHA256 verification + herdr plugin manifest

## License

MIT
