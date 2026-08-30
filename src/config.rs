use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::exec;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    #[default]
    Notify,
    Auto,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub policy: Policy,
    pub check_core: bool,
    pub check_plugins: bool,
    pub startup_check: bool,
    pub require_fast_forward: bool,
    pub immutable_pins: bool,
    pub allow_protocol_change: bool,
    pub allow: Vec<String>,
    pub trusted_owners: Vec<String>,
    pub max_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            policy: Policy::Notify,
            check_core: true,
            check_plugins: true,
            startup_check: true,
            require_fast_forward: true,
            immutable_pins: true,
            allow_protocol_change: false,
            allow: Vec::new(),
            trusted_owners: Vec::new(),
            max_concurrency: 8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub value: Config,
    pub path: PathBuf,
    pub existed: bool,
}

pub fn resolve_dir(herdr_bin: &str, timeout: Duration) -> PathBuf {
    for name in ["HERDR_PLUGIN_CONFIG_DIR", "HERDR_CONFIG_DIR"] {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            return PathBuf::from(value);
        }
    }

    if let Ok(out) = exec::run(
        herdr_bin,
        &["plugin", "config-dir", "herdr-updater"],
        timeout,
    ) {
        if out.ok() && !out.trimmed().is_empty() {
            return PathBuf::from(out.trimmed());
        }
    }

    if cfg!(windows) {
        if let Some(base) = std::env::var_os("APPDATA") {
            return PathBuf::from(base)
                .join("herdr")
                .join("plugins")
                .join("config")
                .join("herdr-updater");
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config")
        .join("herdr")
        .join("plugins")
        .join("config")
        .join("herdr-updater")
}

pub fn load(
    override_path: Option<&Path>,
    herdr_bin: &str,
    timeout: Duration,
) -> Result<Loaded, String> {
    let path = override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| resolve_dir(herdr_bin, timeout).join("config.toml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Loaded {
                value: Config::default(),
                path,
                existed: false,
            });
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut value: Config =
        toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    if !value.require_fast_forward {
        return Err(format!(
            "{} attempts to disable fast-forward-only enforcement",
            path.display()
        ));
    }
    value.max_concurrency = value.max_concurrency.clamp(1, 32);
    Ok(Loaded {
        value,
        path,
        existed: true,
    })
}

impl Config {
    pub fn target_allowed(&self, owner: &str, repo: &str) -> bool {
        if !self.trusted_owners.is_empty()
            && !self
                .trusted_owners
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(owner))
        {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        let target = format!("{owner}/{repo}");
        self.allow.iter().any(|pattern| wildcard(pattern, &target))
    }
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    let (mut pi, mut vi, mut star, mut backtrack) = (0, 0, None, 0);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi].eq_ignore_ascii_case(&v[vi])) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            backtrack = vi;
        } else if let Some(star_index) = star {
            pi = star_index + 1;
            backtrack += 1;
            vi = backtrack;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_non_mutating() {
        let cfg = Config::default();
        assert_eq!(cfg.policy, Policy::Notify);
        assert!(!cfg.allow_protocol_change);
        assert!(cfg.require_fast_forward);
    }

    #[test]
    fn allowlist_and_owner_rules_both_apply() {
        let cfg = Config {
            allow: vec!["diegopzz/herdr-*".into()],
            trusted_owners: vec!["diegopzz".into()],
            ..Config::default()
        };
        assert!(cfg.target_allowed("diegopzz", "herdr-mirror"));
        assert!(!cfg.target_allowed("somebody", "herdr-mirror"));
        assert!(!cfg.target_allowed("diegopzz", "unrelated"));
    }

    #[test]
    fn wildcard_is_case_insensitive_and_bounded_to_the_value() {
        assert!(wildcard("DiegoPzz/herdr-*", "diegopzz/herdr-updater"));
        assert!(wildcard("*/herdr-?", "x/herdr-a"));
        assert!(!wildcard("*/herdr-?", "x/herdr-ab"));
    }

    #[test]
    fn config_cannot_disable_fast_forward_enforcement() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-updater-config-{}-{}.toml",
            std::process::id(),
            unique
        ));
        std::fs::write(&path, "require_fast_forward = false\n").unwrap();
        let result = load(Some(&path), "herdr", Duration::from_secs(1));
        let _ = std::fs::remove_file(path);
        assert!(matches!(result, Err(error) if error.contains("fast-forward-only")));
    }
}
