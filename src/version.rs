//! Version comparison, shared by core update decisions and plugin
//! compatibility checks.
//!
//! This exists because `!=` is not a version comparison. Reading "the manifest
//! says something different, therefore update" is how a tool that promises to
//! keep you current quietly *downgrades* you the first time upstream rolls a
//! release back, or the first time a machine runs something newer than the
//! stable manifest describes. Every other update path in this crate refuses to
//! move backwards; core has to refuse the same way, and refusing needs an
//! ordering, not an inequality.
//!
//! The grammar is semver precedence with build metadata ignored: `MAJOR.MINOR
//! .PATCH` optionally followed by `-PRERELEASE`, where dot-separated numeric
//! identifiers compare numerically, everything else compares as ASCII, numeric
//! sorts below alphanumeric, and a prerelease sorts below the release it leads
//! to. A leading `v` is accepted because tags carry one and manifests do not.
//!
//! Anything that does not fit that grammar parses to `None`, and every caller
//! treats `None` as *unknown* rather than as an ordering. Unknown holds.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: (u64, u64, u64),
    pre: Vec<PreRelease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreRelease {
    Numeric(u64),
    Alphanumeric(String),
}

impl PartialOrd for PreRelease {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PreRelease {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreRelease::Numeric(a), PreRelease::Numeric(b)) => a.cmp(b),
            (PreRelease::Alphanumeric(a), PreRelease::Alphanumeric(b)) => a.cmp(b),
            // "Numeric identifiers always have lower precedence than
            // non-numeric identifiers" — semver 2.0.0 §11.4.3.
            (PreRelease::Numeric(_), PreRelease::Alphanumeric(_)) => Ordering::Less,
            (PreRelease::Alphanumeric(_), PreRelease::Numeric(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            // A release outranks any prerelease of the same core version, so
            // "no prerelease" cannot be compared as an empty list.
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

/// Parse a version, or `None` when it does not fit the supported grammar.
pub fn parse(value: &str) -> Option<Version> {
    let value = value.trim();
    let value = value.strip_prefix('v').unwrap_or(value);
    // Build metadata never affects precedence, so it is discarded rather than
    // making an otherwise-valid version unparseable.
    let value = value.split('+').next()?;
    let (core, pre) = match value.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (value, None),
    };
    let mut parts = core.split('.');
    let major = number(parts.next()?)?;
    let minor = number(parts.next()?)?;
    let patch = number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let pre = match pre {
        None => Vec::new(),
        Some("") => return None,
        Some(pre) => pre
            .split('.')
            .map(|identifier| {
                if identifier.is_empty() {
                    return None;
                }
                if identifier.bytes().all(|byte| byte.is_ascii_digit()) {
                    // Leading zeroes are not a valid numeric identifier, and
                    // guessing which one was meant is exactly the sort of
                    // quiet reinterpretation this module exists to avoid.
                    if identifier.len() > 1 && identifier.starts_with('0') {
                        return None;
                    }
                    return identifier.parse().ok().map(PreRelease::Numeric);
                }
                identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    .then(|| PreRelease::Alphanumeric(identifier.to_string()))
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(Version {
        core: (major, minor, patch),
        pre,
    })
}

fn number(raw: &str) -> Option<u64> {
    if raw.is_empty() || (raw.len() > 1 && raw.starts_with('0')) {
        return None;
    }
    raw.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| raw.parse().ok())
        .flatten()
}

/// Order two version strings, or `None` when either side is unparseable.
///
/// Callers must not fall back to string equality on `None`. "We could not tell"
/// and "they are the same" are different answers, and only one of them is safe
/// to act on.
pub fn compare(left: &str, right: &str) -> Option<Ordering> {
    Some(parse(left)?.cmp(&parse(right)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_release_components_numerically_not_lexically() {
        // The bug string comparison hides: "0.10.0" sorts before "0.9.0" as
        // text, and after it as a version.
        assert_eq!(compare("0.10.0", "0.9.0"), Some(Ordering::Greater));
        assert_eq!(compare("1.0.0", "1.0.1"), Some(Ordering::Less));
        assert_eq!(compare("0.8.2", "0.8.2"), Some(Ordering::Equal));
    }

    #[test]
    fn a_prerelease_sorts_below_the_release_it_leads_to() {
        assert_eq!(compare("0.9.0-rc.1", "0.9.0"), Some(Ordering::Less));
        assert_eq!(compare("0.9.0-rc.1", "0.9.0-rc.2"), Some(Ordering::Less));
        assert_eq!(compare("0.9.0-alpha", "0.9.0-beta"), Some(Ordering::Less));
        // Numeric identifiers rank below alphanumeric ones.
        assert_eq!(compare("0.9.0-1", "0.9.0-alpha"), Some(Ordering::Less));
    }

    #[test]
    fn accepts_a_tag_prefix_and_ignores_build_metadata() {
        assert_eq!(compare("v1.2.3", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare("1.2.3+build.7", "1.2.3"), Some(Ordering::Equal));
    }

    #[test]
    fn unparseable_input_is_unknown_rather_than_an_ordering() {
        assert_eq!(compare("0.8", "0.8.2"), None);
        assert_eq!(compare("nightly", "0.8.2"), None);
        assert_eq!(compare("0.8.2.1", "0.8.2"), None);
        assert_eq!(compare("0.8.2-", "0.8.2"), None);
        assert!(parse("01.2.3").is_none());
    }

    #[test]
    fn equal_versions_written_differently_still_compare_equal() {
        assert_eq!(compare("v0.8.2+meta", "0.8.2"), Some(Ordering::Equal));
    }
}
