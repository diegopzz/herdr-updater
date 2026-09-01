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
    pub check_interval: String,
    pub catalog_refresh_interval: String,
    pub fleet_sync_interval: String,
    pub initial_delay: String,
    pub jitter: String,
    pub quiet_hours: Option<String>,
    pub scheduled_fleet_sync: bool,
    pub sync_update_settings: bool,
    pub require_fast_forward: bool,
    pub immutable_pins: bool,
    pub allow_protocol_change: bool,
    pub allow_channel_mismatch: bool,
    pub allow: Vec<String>,
    pub trusted_owners: Vec<String>,
    pub max_concurrency: usize,
    pub ref_cache_ttl: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            policy: Policy::Notify,
            check_core: true,
            check_plugins: true,
            startup_check: true,
            check_interval: "6h".into(),
            catalog_refresh_interval: "30m".into(),
            fleet_sync_interval: "6h".into(),
            initial_delay: "5m".into(),
            jitter: "5m".into(),
            quiet_hours: None,
            scheduled_fleet_sync: false,
            sync_update_settings: false,
            require_fast_forward: true,
            immutable_pins: true,
            allow_protocol_change: false,
            allow_channel_mismatch: false,
            allow: Vec::new(),
            trusted_owners: Vec::new(),
            max_concurrency: 8,
            // Short on purpose: this is the only cached value that can go
            // stale, and its worst case is a delayed update, never a wrong
            // one. "0s" disables ref caching entirely.
            ref_cache_ttl: "15m".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub value: Config,
    pub path: PathBuf,
    pub existed: bool,
    /// Non-fatal problems found while reading the file — today, keys this
    /// build does not know. Carried rather than printed here so every command
    /// surfaces them in its own format, including `--json`.
    pub warnings: Vec<String>,
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
                warnings: Vec::new(),
            });
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let warnings = inspect_keys(&text, &path)?;
    let mut value: Config =
        toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    value.max_concurrency = value.max_concurrency.clamp(1, 32);
    validate_value(&value, &path)?;
    Ok(Loaded {
        value,
        path,
        existed: true,
        warnings,
    })
}

/// Every key this build understands, taken from the struct itself so the list
/// cannot fall behind the fields.
fn known_keys() -> Vec<String> {
    match serde_json::to_value(Config::default()) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// Classify keys the config file declares that this build does not know.
///
/// serde's `default` attribute means an unrecognised key parses cleanly and is
/// dropped, so `trusted_owner = ["diegopzz"]` silently means *no owner
/// restriction at all* and `polciy = "auto"` silently means notify. A config
/// that quietly does the opposite of what it says is the same class of failure
/// as a check that quietly reports green.
///
/// The two cases are not the same, though, and treating them the same is why
/// this is not simply `deny_unknown_fields`:
///
/// * A key that is *nearly* a known one is a typo. Nothing else explains it,
///   the intended setting is not in effect, and refusing to run is the only
///   answer that cannot be ignored.
/// * A key that resembles nothing is most likely a setting from a newer build,
///   read by an older one — which happens routinely on a fleet mid-rollout, and
///   must not brick the older hosts. That warns.
fn inspect_keys(text: &str, path: &Path) -> Result<Vec<String>, String> {
    let Ok(table) = text.parse::<toml::Table>() else {
        // A real parse error is reported by the typed parse, with a better
        // message than anything this function could add.
        return Ok(Vec::new());
    };
    let known = known_keys();
    let mut warnings = Vec::new();
    let mut typos = Vec::new();
    for key in table.keys() {
        if known.iter().any(|candidate| candidate == key) {
            continue;
        }
        match nearest_key(key, &known) {
            Some(suggestion) => typos.push(format!("{key:?} (did you mean {suggestion:?}?)")),
            None => warnings.push(format!(
                "{}: unknown setting {key:?} was ignored; it may belong to a newer herdr-updater",
                path.display()
            )),
        }
    }
    if !typos.is_empty() {
        return Err(format!(
            "{}: {} looks like a misspelled setting and would be silently ignored: {}",
            path.display(),
            if typos.len() == 1 {
                "this key"
            } else {
                "these keys"
            },
            typos.join(", ")
        ));
    }
    Ok(warnings)
}

/// The closest known key within a small edit distance, or `None` when nothing
/// is close enough for a typo to be the likely explanation.
fn nearest_key(key: &str, known: &[String]) -> Option<String> {
    // Case and separator slips are typos regardless of length.
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    if let Some(exact) = known.iter().find(|candidate| **candidate == normalized) {
        return Some(exact.clone());
    }
    // Below four characters an edit distance of two is most of the word, so the
    // "suggestion" would be noise.
    if key.len() < 4 {
        return None;
    }
    let tolerance = if key.len() <= 6 { 1 } else { 2 };
    known
        .iter()
        .map(|candidate| (edit_distance(&normalized, candidate), candidate))
        .filter(|(distance, _)| *distance <= tolerance)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate.clone())
}

/// Levenshtein distance over bytes, with a single rolling row. Config keys are
/// ASCII and short, so this costs nothing worth measuring.
fn edit_distance(left: &str, right: &str) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, l) in left.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, r) in right.iter().enumerate() {
            let next = row[j + 1];
            row[j + 1] = if l == r {
                diagonal
            } else {
                1 + diagonal.min(row[j]).min(row[j + 1])
            };
            diagonal = next;
        }
    }
    row[right.len()]
}

impl Config {
    pub fn check_every(&self) -> Duration {
        parse_duration(&self.check_interval).unwrap_or(Duration::from_secs(6 * 60 * 60))
    }

    pub fn catalog_refresh_every(&self) -> Duration {
        parse_duration(&self.catalog_refresh_interval).unwrap_or(Duration::from_secs(30 * 60))
    }

    pub fn fleet_sync_every(&self) -> Duration {
        parse_duration(&self.fleet_sync_interval).unwrap_or(Duration::from_secs(6 * 60 * 60))
    }

    pub fn initial_delay_value(&self) -> Duration {
        parse_duration(&self.initial_delay).unwrap_or(Duration::from_secs(5 * 60))
    }

    pub fn jitter_value(&self) -> Duration {
        parse_duration(&self.jitter).unwrap_or(Duration::from_secs(5 * 60))
    }

    /// Seconds a resolved ref may be reused. Unparseable falls back to the
    /// default rather than to "forever".
    pub fn ref_cache_seconds(&self) -> u64 {
        parse_duration(&self.ref_cache_ttl)
            .unwrap_or(Duration::from_secs(15 * 60))
            .as_secs()
            .min(6 * 60 * 60)
    }

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

pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("duration is empty".into());
    }
    let bytes = value.as_bytes();
    let mut index = 0usize;
    let mut total = 0u64;
    while index < bytes.len() {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index || index == bytes.len() {
            return Err(format!(
                "invalid duration {value:?}; use values such as 30m, 6h, or 1d"
            ));
        }
        let amount: u64 = value[start..index]
            .parse()
            .map_err(|_| format!("duration component is too large in {value:?}"))?;
        let multiplier = match bytes[index].to_ascii_lowercase() {
            b's' => 1,
            b'm' => 60,
            b'h' => 60 * 60,
            b'd' => 24 * 60 * 60,
            _ => {
                return Err(format!(
                    "invalid duration unit in {value:?}; supported units are s, m, h, and d"
                ))
            }
        };
        total = total
            .checked_add(
                amount
                    .checked_mul(multiplier)
                    .ok_or_else(|| format!("duration component is too large in {value:?}"))?,
            )
            .ok_or_else(|| format!("duration is too large in {value:?}"))?;
        index += 1;
    }
    Ok(Duration::from_secs(total))
}

pub(crate) fn validate_value(value: &Config, path: &Path) -> Result<(), String> {
    if !value.require_fast_forward {
        return Err(format!(
            "{} attempts to disable fast-forward-only enforcement",
            path.display()
        ));
    }
    validate_durations(value, path)
}

fn validate_durations(value: &Config, path: &Path) -> Result<(), String> {
    let check = bounded_duration(
        &value.check_interval,
        60,
        30 * 24 * 60 * 60,
        "check_interval",
        path,
    )?;
    bounded_duration(
        &value.catalog_refresh_interval,
        5 * 60,
        7 * 24 * 60 * 60,
        "catalog_refresh_interval",
        path,
    )?;
    bounded_duration(
        &value.fleet_sync_interval,
        5 * 60,
        30 * 24 * 60 * 60,
        "fleet_sync_interval",
        path,
    )?;
    bounded_duration(
        &value.initial_delay,
        0,
        7 * 24 * 60 * 60,
        "initial_delay",
        path,
    )?;
    let jitter = bounded_duration(&value.jitter, 0, 24 * 60 * 60, "jitter", path)?;
    if jitter > check {
        return Err(format!(
            "{}: jitter cannot exceed check_interval",
            path.display()
        ));
    }
    if let Some(hours) = value.quiet_hours.as_deref() {
        parse_quiet_hours(hours).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn fingerprint(value: &Config) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn bounded_duration(
    raw: &str,
    minimum: u64,
    maximum: u64,
    field: &str,
    path: &Path,
) -> Result<Duration, String> {
    let parsed =
        parse_duration(raw).map_err(|error| format!("{}: {field}: {error}", path.display()))?;
    if parsed.as_secs() < minimum || parsed.as_secs() > maximum {
        return Err(format!(
            "{}: {field} must be between {minimum}s and {maximum}s",
            path.display()
        ));
    }
    Ok(parsed)
}

pub fn parse_quiet_hours(value: &str) -> Result<(u16, u16), String> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| "quiet_hours must use HH:MM-HH:MM".to_string())?;
    fn minutes(raw: &str) -> Option<u16> {
        let (hour, minute) = raw.split_once(':')?;
        if hour.len() != 2 || minute.len() != 2 {
            return None;
        }
        let hour: u16 = hour.parse().ok()?;
        let minute: u16 = minute.parse().ok()?;
        (hour < 24 && minute < 60).then_some(hour * 60 + minute)
    }
    let start =
        minutes(start).ok_or_else(|| "quiet_hours has an invalid start time".to_string())?;
    let end = minutes(end).ok_or_else(|| "quiet_hours has an invalid end time".to_string())?;
    if start == end {
        return Err("quiet_hours cannot cover the entire day".into());
    }
    Ok((start, end))
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

    #[test]
    fn durations_support_compound_values_and_require_units() {
        assert_eq!(parse_duration("1h30m").unwrap().as_secs(), 5_400);
        assert_eq!(parse_duration("2d3h4m5s").unwrap().as_secs(), 183_845);
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("1h 30m").is_err());
    }

    #[test]
    fn quiet_hours_validate_clock_ranges() {
        assert_eq!(parse_quiet_hours("22:00-07:30").unwrap(), (1_320, 450));
        assert!(parse_quiet_hours("24:00-07:30").is_err());
        assert!(parse_quiet_hours("07:30-07:30").is_err());
    }

    fn temp_config(contents: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-updater-keys-{}-{unique}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn known_keys_are_taken_from_the_struct_itself() {
        let keys = known_keys();
        for expected in ["policy", "trusted_owners", "allow_channel_mismatch"] {
            assert!(keys.iter().any(|key| key == expected), "missing {expected}");
        }
    }

    #[test]
    fn a_misspelled_setting_is_an_error_not_a_silent_default() {
        // The failure this prevents: `trusted_owner` parses clean, and the
        // owner restriction the operator wrote is simply not in effect.
        let path = temp_config("trusted_owner = [\"diegopzz\"]\n");
        let result = load(Some(&path), "herdr", Duration::from_secs(1));
        let _ = std::fs::remove_file(&path);
        let error = result.expect_err("a near-miss key must fail closed");
        assert!(error.contains("trusted_owner"), "{error}");
        assert!(error.contains("trusted_owners"), "{error}");
    }

    #[test]
    fn a_key_from_a_newer_build_warns_instead_of_bricking_an_older_host() {
        let path = temp_config("some_future_capability = true\n");
        let result = load(Some(&path), "herdr", Duration::from_secs(1));
        let _ = std::fs::remove_file(&path);
        let loaded = result.expect("an unrecognisable key must not fail the run");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("some_future_capability"));
        assert_eq!(loaded.value.policy, Policy::Notify);
    }

    #[test]
    fn a_valid_config_produces_no_warnings() {
        let path = temp_config("policy = \"auto\"\ntrusted_owners = [\"diegopzz\"]\n");
        let result = load(Some(&path), "herdr", Duration::from_secs(1));
        let _ = std::fs::remove_file(&path);
        let loaded = result.expect("a valid config must load");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.value.policy, Policy::Auto);
    }

    #[test]
    fn case_and_separator_slips_are_treated_as_typos() {
        assert_eq!(
            nearest_key("Check-Interval", &known_keys()).as_deref(),
            Some("check_interval")
        );
        assert_eq!(nearest_key("zzz", &known_keys()), None);
    }

    #[test]
    fn edit_distance_matches_known_values() {
        assert_eq!(edit_distance("policy", "policy"), 0);
        assert_eq!(edit_distance("polciy", "policy"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn jitter_cannot_exceed_the_check_interval() {
        let value = Config {
            check_interval: "10m".into(),
            jitter: "11m".into(),
            ..Config::default()
        };
        assert!(validate_value(&value, Path::new("config.toml"))
            .unwrap_err()
            .contains("jitter cannot exceed"));
    }
}
