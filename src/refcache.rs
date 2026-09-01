//! Caching for the two network answers a plugin check needs.
//!
//! Caching was argued against for this crate, on the grounds that its whole
//! value is never reporting a stale green. That objection is right about one
//! of these two lookups and wrong about the other, and the difference is what
//! makes this safe to have at all:
//!
//! * **`compare(installed, remote)` is a pure function of two immutable
//!   commits.** GitHub cannot change the ancestry between two fixed SHAs. Its
//!   answer is therefore valid forever, and caching it carries no staleness
//!   risk whatsoever. This is also the expensive one — it is the call that
//!   spends the API budget that `Relation::RateLimited` reports, one request
//!   per GitHub-sourced plugin per check.
//! * **`ls-remote(ref)` is time-varying**, because a branch tip moves. Caching
//!   it can only ever produce one error: reporting "current" when an update
//!   landed inside the TTL. That is a *delayed* update, never a wrong one — the
//!   apply path re-resolves and `plugins::verify` checks the commit it actually
//!   installed — but it is real, so it is bounded by a short TTL, disabled by
//!   `--refresh`, and skipped entirely when a mutation is about to happen.
//!
//! So the permanent half is free and the risky half is bounded and bypassable.
//! An entry is only ever a saved answer to a question already asked; nothing
//! here decides anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CACHE_FILE: &str = "ref-cache-v1.json";
/// Bounded so a corrupt or runaway file cannot become a memory problem, and so
/// the whole thing stays cheap to parse on every run.
const MAX_ENTRIES: usize = 2_000;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Whether this run may read time-varying entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ordinary read-only inspection: both caches are live.
    Use,
    /// `--refresh`, or anything about to mutate. Immutable compare answers are
    /// still used — they cannot be stale — but every ref is re-resolved.
    Fresh,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Entry {
    value: String,
    unix_seconds: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Disk {
    /// ref → resolved sha, time-varying, TTL'd.
    #[serde(default)]
    refs: BTreeMap<String, Entry>,
    /// (installed…remote) → relation, immutable, never expires.
    #[serde(default)]
    compares: BTreeMap<String, Entry>,
}

/// Read-only snapshot plus the updates this run produced.
#[derive(Debug)]
pub struct RefCache {
    disk: Disk,
    mode: Mode,
    ttl_seconds: u64,
    fresh: Mutex<Disk>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join(CACHE_FILE)
}

impl RefCache {
    /// Load the cache for this run. A missing, oversized, or unparseable file
    /// is simply an empty cache — a cache that cannot be read must never be an
    /// error, because it is an optimisation and nothing depends on it.
    pub fn load(config_dir: &Path, mode: Mode, ttl_seconds: u64) -> Self {
        let disk = std::fs::metadata(path(config_dir))
            .ok()
            .filter(|meta| meta.is_file() && meta.len() <= MAX_BYTES)
            .and_then(|_| std::fs::read(path(config_dir)).ok())
            .and_then(|bytes| serde_json::from_slice::<Disk>(&bytes).ok())
            .unwrap_or_default();
        Self {
            disk,
            mode,
            ttl_seconds,
            fresh: Mutex::new(Disk::default()),
        }
    }

    /// A cache that never hits and never stores, for callers with nowhere to
    /// put one.
    pub fn disabled() -> Self {
        Self {
            disk: Disk::default(),
            mode: Mode::Fresh,
            ttl_seconds: 0,
            fresh: Mutex::new(Disk::default()),
        }
    }

    /// The resolved sha for a ref, if it was cached recently enough.
    pub fn remote_ref(&self, key: &str) -> Option<String> {
        if self.mode == Mode::Fresh || self.ttl_seconds == 0 {
            return None;
        }
        let entry = self.disk.refs.get(key)?;
        // A future timestamp means the clock moved; treat it as expired rather
        // than trusting it for however long the skew lasts.
        let age = now().checked_sub(entry.unix_seconds)?;
        (age <= self.ttl_seconds).then(|| entry.value.clone())
    }

    pub fn store_remote_ref(&self, key: &str, sha: &str) {
        if self.ttl_seconds == 0 {
            return;
        }
        if let Ok(mut fresh) = self.fresh.lock() {
            fresh.refs.insert(
                key.to_string(),
                Entry {
                    value: sha.to_string(),
                    unix_seconds: now(),
                },
            );
        }
    }

    /// The relation between two commits. Never expires: the ancestry between
    /// two immutable SHAs is not something that can change later.
    pub fn compare(&self, key: &str) -> Option<String> {
        self.disk.compares.get(key).map(|entry| entry.value.clone())
    }

    pub fn store_compare(&self, key: &str, relation: &str) {
        if let Ok(mut fresh) = self.fresh.lock() {
            fresh.compares.insert(
                key.to_string(),
                Entry {
                    value: relation.to_string(),
                    unix_seconds: now(),
                },
            );
        }
    }

    /// Merge this run's answers over the loaded ones and write them out. Called
    /// once, from the main thread, after the parallel inspection finishes — so
    /// concurrent workers never race on the file.
    pub fn persist(&self, config_dir: &Path) {
        let Ok(fresh) = self.fresh.lock() else { return };
        if fresh.refs.is_empty() && fresh.compares.is_empty() {
            return;
        }
        let mut merged = Disk {
            refs: self.disk.refs.clone(),
            compares: self.disk.compares.clone(),
        };
        merged.refs.extend(fresh.refs.clone());
        merged.compares.extend(fresh.compares.clone());
        prune(&mut merged.refs);
        prune(&mut merged.compares);
        if let Ok(bytes) = serde_json::to_vec(&merged) {
            if let Some(parent) = path(config_dir).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Best effort throughout: a cache that cannot be written costs a
            // little speed and nothing else.
            let _ = std::fs::write(path(config_dir), bytes);
        }
    }
}

/// Drop the oldest entries once the map outgrows its bound.
fn prune(entries: &mut BTreeMap<String, Entry>) {
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    let mut by_age: Vec<(String, u64)> = entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.unix_seconds))
        .collect();
    by_age.sort_by_key(|(_, seconds)| *seconds);
    for (key, _) in by_age.into_iter().take(entries.len() - MAX_ENTRIES) {
        entries.remove(&key);
    }
}

/// Key for a ref lookup. Includes the ref so a repo tracked at two refs does
/// not collide.
pub fn ref_key(owner: &str, repo: &str, reference: Option<&str>) -> String {
    format!("{owner}/{repo}@{}", reference.unwrap_or("HEAD"))
}

/// Key for a commit comparison. Both ends are full SHAs, which is what makes
/// the answer permanent.
pub fn compare_key(owner: &str, repo: &str, installed: &str, remote: &str) -> String {
    format!("{owner}/{repo}:{installed}...{remote}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let unique = now();
        let dir = std::env::temp_dir().join(format!(
            "herdr-updater-refcache-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_compare_answer_never_expires_because_commits_do_not_move() {
        let dir = scratch("compare");
        let cache = RefCache::load(&dir, Mode::Use, 60);
        let key = compare_key("o", "r", &"a".repeat(40), &"b".repeat(40));
        cache.store_compare(&key, "behind");
        cache.persist(&dir);

        // Reloaded with a zero TTL and in Fresh mode — the settings that
        // disable ref caching entirely — the compare answer must still hit.
        let reloaded = RefCache::load(&dir, Mode::Fresh, 0);
        assert_eq!(reloaded.compare(&key).as_deref(), Some("behind"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_ref_answer_expires_and_is_skipped_when_fresh_is_demanded() {
        let dir = scratch("refs");
        let cache = RefCache::load(&dir, Mode::Use, 3_600);
        let key = ref_key("o", "r", Some("main"));
        cache.store_remote_ref(&key, &"c".repeat(40));
        cache.persist(&dir);

        assert!(
            RefCache::load(&dir, Mode::Use, 3_600)
                .remote_ref(&key)
                .is_some(),
            "a ref inside its TTL should hit"
        );
        assert!(
            RefCache::load(&dir, Mode::Use, 0)
                .remote_ref(&key)
                .is_none(),
            "a zero TTL disables ref caching"
        );
        assert!(
            RefCache::load(&dir, Mode::Fresh, 3_600)
                .remote_ref(&key)
                .is_none(),
            "--refresh and mutations must always re-resolve"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_cache_is_an_empty_cache_not_an_error() {
        let dir = scratch("corrupt");
        std::fs::write(path(&dir), b"{ not json").unwrap();
        let cache = RefCache::load(&dir, Mode::Use, 60);
        assert!(cache.remote_ref(&ref_key("o", "r", None)).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_separate_repos_refs_and_commit_pairs() {
        assert_ne!(
            ref_key("o", "r", Some("main")),
            ref_key("o", "r", Some("dev"))
        );
        assert_ne!(ref_key("o", "r", None), ref_key("o", "x", None));
        assert_ne!(
            compare_key("o", "r", "a", "b"),
            compare_key("o", "r", "b", "a")
        );
    }

    #[test]
    fn the_entry_bound_drops_the_oldest_first() {
        let mut entries = BTreeMap::new();
        for index in 0..(MAX_ENTRIES + 10) {
            entries.insert(
                format!("k{index:05}"),
                Entry {
                    value: "v".into(),
                    unix_seconds: index as u64,
                },
            );
        }
        prune(&mut entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(!entries.contains_key("k00000"), "oldest must go first");
        assert!(entries.contains_key(&format!("k{:05}", MAX_ENTRIES + 9)));
    }
}
