use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::exec;

#[derive(Debug, Clone, Deserialize)]
struct PluginListEnvelope {
    result: PluginListResult,
}

#[derive(Debug, Clone, Deserialize)]
struct PluginListResult {
    plugins: Vec<InstalledPlugin>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InstalledPlugin {
    pub plugin_id: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub min_herdr_version: Option<String>,
    #[serde(default)]
    pub plugin_root: Option<String>,
    #[serde(default)]
    pub source: Option<PluginSource>,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PluginSource {
    pub kind: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub subdir: Option<String>,
    #[serde(default)]
    pub requested_ref: Option<String>,
    #[serde(default)]
    pub resolved_commit: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    DefaultBranch,
    Branch,
    Tag,
    Commit,
    Linked,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Same,
    Behind,
    Ahead,
    Diverged,
    /// GitHub refused to answer because the API budget is spent. Distinct from
    /// `Unknown` because the remedy is specific and the wait is finite: with
    /// `gh` unauthenticated the fallback gets 60 requests an hour for the whole
    /// machine, and one inspection costs one request per plugin. Reported as an
    /// error, not a quiet hold, so a fleet does not read as current the moment
    /// it grows past the budget.
    RateLimited,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginState {
    pub plugin_id: String,
    pub version: Option<String>,
    pub plugin_root: Option<String>,
    pub source_kind: String,
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub subdir: Option<String>,
    pub requested_ref: Option<String>,
    pub installed_sha: Option<String>,
    pub remote_sha: Option<String>,
    pub channel: Channel,
    pub relation: Relation,
    pub update_available: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "decision", content = "reason")]
pub enum Decision {
    Current,
    Update,
    Hold(String),
    Error(String),
}

pub(crate) fn valid_segment(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub(crate) fn valid_subdir(value: &str) -> bool {
    value.len() <= 240
        && !value.starts_with(['/', '\\'])
        && value
            .split(['/', '\\'])
            .all(|part| valid_segment(part, 100))
}

pub(crate) fn full_commit(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn valid_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !matches!(value.as_bytes().first(), Some(b'-' | b'/' | b'.'))
        && !matches!(value.as_bytes().last(), Some(b'/' | b'.'))
        && !value.ends_with(".lock")
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
        && value.bytes().all(|byte| {
            !byte.is_ascii_control()
                && !matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

pub(crate) fn list_installed(
    herdr_bin: &str,
    timeout: Duration,
) -> Result<Vec<InstalledPlugin>, String> {
    let out = exec::run(herdr_bin, &["plugin", "list", "--json"], timeout)
        .map_err(|e| format!("herdr plugin list: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "herdr plugin list exited {}: {}",
            out.code,
            out.stderr.lines().next().unwrap_or("no stderr")
        ));
    }
    let parsed: PluginListEnvelope = serde_json::from_str(&out.stdout)
        .map_err(|e| format!("herdr plugin list --json is not valid JSON: {e}"))?;
    Ok(parsed.result.plugins)
}

fn ref_sha(output: &str, suffix: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (sha, name) = line.split_once(char::is_whitespace)?;
        (name.trim() == suffix && full_commit(sha)).then(|| sha.to_ascii_lowercase())
    })
}

fn resolve_remote(
    owner: &str,
    repo: &str,
    requested_ref: Option<&str>,
    installed: &str,
    timeout: Duration,
) -> Result<(String, Channel), String> {
    if requested_ref.is_some_and(full_commit) {
        return Ok((installed.to_ascii_lowercase(), Channel::Commit));
    }
    let url = format!("https://github.com/{owner}/{repo}.git");
    if let Some(reference) = requested_ref.filter(|value| !value.is_empty()) {
        if !valid_ref(reference) {
            return Err(format!(
                "ref contains unsupported characters: {reference:?}"
            ));
        }
        let head = format!("refs/heads/{reference}");
        let tag = format!("refs/tags/{reference}");
        let peeled = format!("{tag}^{{}}");
        let args = ["ls-remote", &url, &head, &tag, &peeled];
        let out = exec::run("git", &args, timeout).map_err(|e| format!("git ls-remote: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "git ls-remote exited {}: {}",
                out.code,
                out.stderr.lines().next().unwrap_or("no stderr")
            ));
        }
        let branch_sha = ref_sha(&out.stdout, &head);
        let tag_sha = ref_sha(&out.stdout, &peeled).or_else(|| ref_sha(&out.stdout, &tag));
        return match (branch_sha, tag_sha) {
            (Some(_), Some(_)) => Err(format!(
                "ref {reference:?} is both a branch and tag; pin an exact commit"
            )),
            (Some(sha), None) => Ok((sha, Channel::Branch)),
            (None, Some(sha)) => Ok((sha, Channel::Tag)),
            (None, None) => Err(format!("upstream ref {reference:?} does not exist")),
        };
    }

    let out = exec::run("git", &["ls-remote", "--symref", &url, "HEAD"], timeout)
        .map_err(|e| format!("git ls-remote: {e}"))?;
    if !out.ok() {
        return Err(format!(
            "git ls-remote exited {}: {}",
            out.code,
            out.stderr.lines().next().unwrap_or("no stderr")
        ));
    }
    ref_sha(&out.stdout, "HEAD")
        .map(|sha| (sha, Channel::DefaultBranch))
        .ok_or_else(|| "upstream default branch did not resolve to a commit".to_string())
}

fn relation_from_status(status: &str) -> Option<Relation> {
    match status.trim_matches(['\r', '\n', '"']) {
        "identical" => Some(Relation::Same),
        // GitHub compares base...head. The remote SHA is the head.
        "ahead" => Some(Relation::Behind),
        "behind" => Some(Relation::Ahead),
        "diverged" => Some(Relation::Diverged),
        _ => None,
    }
}

fn compare(
    owner: &str,
    repo: &str,
    installed: &str,
    remote: &str,
    timeout: Duration,
) -> Result<Relation, String> {
    if installed.eq_ignore_ascii_case(remote) {
        return Ok(Relation::Same);
    }
    if !full_commit(installed) || !full_commit(remote) {
        return Err("installed or remote revision is not a full commit SHA".into());
    }
    let endpoint = format!("repos/{owner}/{repo}/compare/{installed}...{remote}");
    let mut rate_limited = false;
    if exec::have("gh") {
        let out = exec::run("gh", &["api", &endpoint, "--jq", ".status"], timeout)
            .map_err(|e| format!("GitHub compare: {e}"))?;
        if out.ok() {
            if let Some(relation) = relation_from_status(out.trimmed()) {
                return Ok(relation);
            }
        } else {
            rate_limited |= mentions_rate_limit(&out.stderr) || mentions_rate_limit(&out.stdout);
        }
    }

    if exec::have("curl") {
        let url = format!("https://api.github.com/{endpoint}");
        let max_time = timeout.as_secs().max(1).to_string();
        let agent = concat!("User-Agent: herdr-updater/", env!("CARGO_PKG_VERSION"));
        // `-f` is deliberately absent: it collapses every HTTP error into exit
        // 22 and discards the body, which is precisely the information needed
        // to tell "rate limited, try again in 40 minutes" from "we have no idea
        // what happened".
        let out = exec::run(
            "curl",
            &[
                "-sSL",
                "--max-time",
                &max_time,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                agent,
                "-w",
                "\n%{http_code}",
                &url,
            ],
            timeout,
        )
        .map_err(|e| format!("GitHub compare fallback: {e}"))?;
        if out.ok() {
            let (body, code) = split_status(&out.stdout);
            if matches!(code, Some(403) | Some(429)) && mentions_rate_limit(body) {
                return Ok(Relation::RateLimited);
            }
            if code == Some(200) {
                let value: serde_json::Value = serde_json::from_str(body)
                    .map_err(|e| format!("GitHub compare response is not JSON: {e}"))?;
                return match value
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .and_then(relation_from_status)
                {
                    Some(relation) => Ok(relation),
                    // A 200 whose body says something we do not recognise is a
                    // different problem from a 200 we never got, and saying
                    // "returned HTTP 200" as if that were the fault sends the
                    // reader to check a network that is working fine.
                    None => {
                        Err("GitHub compare succeeded but reported no recognisable status".into())
                    }
                };
            }
            if let Some(code) = code {
                return Err(format!("GitHub compare returned HTTP {code}"));
            }
        }
    }
    if rate_limited {
        return Ok(Relation::RateLimited);
    }
    Err("could not classify the commit relationship with gh or curl".into())
}

/// Split a `curl -w '\n%{http_code}'` response into its body and status.
fn split_status(response: &str) -> (&str, Option<u16>) {
    match response.trim_end().rsplit_once('\n') {
        Some((body, code)) => (body, code.trim().parse().ok()),
        // A body-less response is all status and no body.
        None => ("", response.trim().parse().ok()),
    }
}

/// GitHub words its budget refusals a few ways — primary limit, secondary
/// limit, and abuse detection — and all three mean "wait", not "unknown".
fn mentions_rate_limit(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("rate limit") || text.contains("secondary rate") || text.contains("api limit")
}

fn inspect_one(plugin: InstalledPlugin, timeout: Duration) -> PluginState {
    let mut state = PluginState {
        plugin_id: plugin.plugin_id,
        version: plugin.version,
        plugin_root: plugin.plugin_root,
        source_kind: plugin
            .source
            .as_ref()
            .map(|s| s.kind.clone())
            .unwrap_or_default(),
        owner: plugin.source.as_ref().and_then(|s| s.owner.clone()),
        repo: plugin.source.as_ref().and_then(|s| s.repo.clone()),
        subdir: plugin.source.as_ref().and_then(|s| s.subdir.clone()),
        requested_ref: plugin.source.as_ref().and_then(|s| s.requested_ref.clone()),
        installed_sha: plugin
            .source
            .as_ref()
            .and_then(|s| s.resolved_commit.clone()),
        remote_sha: None,
        channel: Channel::Unmanaged,
        relation: Relation::NotApplicable,
        update_available: false,
        error: None,
    };
    let Some(source) = plugin.source else {
        return state;
    };
    if source.kind == "local" {
        state.channel = Channel::Linked;
        return state;
    }
    if source.kind != "github" {
        return state;
    }
    let (Some(owner), Some(repo), Some(installed)) = (
        source.owner.as_deref(),
        source.repo.as_deref(),
        source.resolved_commit.as_deref(),
    ) else {
        state.error = Some("GitHub source is missing owner, repo, or resolved commit".into());
        state.relation = Relation::Unknown;
        return state;
    };
    if !valid_segment(owner, 100)
        || !valid_segment(repo, 100)
        || !full_commit(installed)
        || source
            .subdir
            .as_deref()
            .is_some_and(|value| !valid_subdir(value))
    {
        state.error = Some("GitHub source metadata failed validation".into());
        state.relation = Relation::Unknown;
        return state;
    }
    match resolve_remote(
        owner,
        repo,
        source.requested_ref.as_deref(),
        installed,
        timeout,
    ) {
        Ok((remote, channel)) => {
            state.remote_sha = Some(remote.clone());
            state.channel = channel;
            match compare(owner, repo, installed, &remote, timeout) {
                Ok(relation) => {
                    state.relation = relation;
                    state.update_available = relation == Relation::Behind;
                }
                Err(e) => {
                    state.relation = Relation::Unknown;
                    state.error = Some(e);
                }
            }
        }
        Err(e) => {
            state.relation = Relation::Unknown;
            state.error = Some(e);
        }
    }
    state
}

pub fn inspect_all(
    herdr_bin: &str,
    config: &Config,
    only: Option<&str>,
    timeout: Duration,
) -> Result<Vec<PluginState>, String> {
    let mut installed = list_installed(herdr_bin, timeout)?;
    if let Some(id) = only {
        installed.retain(|plugin| plugin.plugin_id == id);
        if installed.is_empty() {
            return Err(format!("plugin {id:?} is not installed"));
        }
    }

    let mut states = Vec::with_capacity(installed.len());
    let (tx, rx) = mpsc::channel();
    for chunk in installed.chunks(config.max_concurrency) {
        let mut handles = Vec::with_capacity(chunk.len());
        for plugin in chunk.iter().cloned() {
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let _ = tx.send(inspect_one(plugin, timeout));
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
    }
    drop(tx);
    states.extend(rx);
    states.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    Ok(states)
}

pub fn decide(state: &PluginState, config: &Config) -> Decision {
    if let Some(error) = &state.error {
        return Decision::Error(error.clone());
    }
    match state.channel {
        Channel::Linked => {
            return Decision::Hold("linked checkout; never replace a local fork".into());
        }
        Channel::Unmanaged => return Decision::Hold("non-GitHub plugin source".into()),
        Channel::Commit | Channel::Tag if config.immutable_pins => {
            return Decision::Hold("immutable pin".into());
        }
        _ => {}
    }
    match state.relation {
        Relation::Same => Decision::Current,
        Relation::Behind => {
            let (Some(owner), Some(repo)) = (&state.owner, &state.repo) else {
                return Decision::Error("missing GitHub identity".into());
            };
            if !config.target_allowed(owner, repo) {
                Decision::Hold("outside allowlist or trusted-owner policy".into())
            } else {
                Decision::Update
            }
        }
        Relation::Ahead => Decision::Hold("local checkout is ahead of upstream".into()),
        Relation::Diverged => Decision::Hold("local and upstream histories diverged".into()),
        Relation::RateLimited => Decision::Error(
            "GitHub API rate limit reached; authenticate gh for a higher budget".into(),
        ),
        Relation::Unknown => Decision::Error("commit relationship is unknown".into()),
        Relation::NotApplicable => Decision::Current,
    }
}

pub fn install(
    herdr_bin: &str,
    state: &PluginState,
    reference: Option<&str>,
    timeout: Duration,
) -> Result<(), String> {
    let (Some(owner), Some(repo)) = (&state.owner, &state.repo) else {
        return Err("plugin has no GitHub source".into());
    };
    let mut source = format!("{owner}/{repo}");
    if let Some(subdir) = &state.subdir {
        source.push('/');
        source.push_str(subdir);
    }
    install_source(herdr_bin, &source, reference, true, timeout)
}

pub fn install_source(
    herdr_bin: &str,
    source: &str,
    reference: Option<&str>,
    yes: bool,
    timeout: Duration,
) -> Result<(), String> {
    validate_source(source)?;
    if reference.is_some_and(|value| !valid_ref(value)) {
        return Err("plugin reference contains unsupported characters".into());
    }
    let mut owned = vec![
        "plugin".to_string(),
        "install".to_string(),
        source.to_string(),
    ];
    if let Some(reference) = reference.filter(|value| !value.is_empty()) {
        owned.push("--ref".into());
        owned.push(reference.into());
    }
    if yes {
        owned.push("--yes".into());
    }
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    if yes {
        let out =
            exec::run(herdr_bin, &args, timeout).map_err(|e| format!("plugin install: {e}"))?;
        if !out.ok() {
            return Err(format!(
                "plugin install exited {}: {}",
                out.code,
                out.stderr.lines().next().unwrap_or("no stderr")
            ));
        }
    } else {
        let code =
            exec::run_inherited(herdr_bin, &args).map_err(|e| format!("plugin install: {e}"))?;
        if code != 0 {
            return Err(format!("plugin install exited {code}"));
        }
    }
    Ok(())
}

pub(crate) fn validate_source(source: &str) -> Result<(), String> {
    if source.len() > 340 || source.starts_with(['/', '\\']) {
        return Err("plugin source is invalid".into());
    }
    let mut parts = source.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if !valid_segment(owner, 100) || !valid_segment(repo, 100) {
        return Err("plugin source must be owner/repo[/subdir]".into());
    }
    let rest = parts.collect::<Vec<_>>().join("/");
    if !rest.is_empty() && !valid_subdir(&rest) {
        return Err("plugin source subdirectory is invalid".into());
    }
    Ok(())
}

pub fn verify(
    herdr_bin: &str,
    plugin_id: &str,
    expected_sha: &str,
    timeout: Duration,
) -> Result<(), String> {
    let plugins = list_installed(herdr_bin, timeout)?;
    let plugin = plugins
        .into_iter()
        .find(|plugin| plugin.plugin_id == plugin_id)
        .ok_or_else(|| format!("{plugin_id} disappeared after install"))?;
    let actual = plugin
        .source
        .and_then(|source| source.resolved_commit)
        .ok_or_else(|| format!("{plugin_id} has no resolved commit after install"))?;
    if !actual.eq_ignore_ascii_case(expected_sha) {
        return Err(format!(
            "{plugin_id} resolved to {actual}, expected {expected_sha}"
        ));
    }
    let out = exec::run(herdr_bin, &["plugin", "action", "list"], timeout)
        .map_err(|e| format!("post-install action discovery: {e}"))?;
    if !out.ok() {
        return Err(format!("post-install action discovery exited {}", out.code));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(channel: Channel, relation: Relation) -> PluginState {
        PluginState {
            plugin_id: "sample".into(),
            version: None,
            plugin_root: None,
            source_kind: "github".into(),
            owner: Some("diegopzz".into()),
            repo: Some("herdr-sample".into()),
            subdir: None,
            requested_ref: None,
            installed_sha: Some("a".repeat(40)),
            remote_sha: Some("b".repeat(40)),
            channel,
            relation,
            update_available: relation == Relation::Behind,
            error: None,
        }
    }

    #[test]
    fn full_shas_are_commit_pins_but_short_hex_can_be_a_branch() {
        assert!(full_commit(&"a".repeat(40)));
        assert!(!full_commit("deadbeef"));
    }

    #[test]
    fn linked_plugins_are_never_update_candidates() {
        assert!(matches!(
            decide(&state(Channel::Linked, Relation::NotApplicable), &Config::default()),
            Decision::Hold(reason) if reason.contains("linked")
        ));
    }

    #[test]
    fn only_behind_branches_are_updateable() {
        assert_eq!(
            decide(
                &state(Channel::Branch, Relation::Behind),
                &Config::default()
            ),
            Decision::Update
        );
        assert!(matches!(
            decide(
                &state(Channel::Branch, Relation::Diverged),
                &Config::default()
            ),
            Decision::Hold(_)
        ));
    }

    #[test]
    fn immutable_tags_are_held() {
        assert!(matches!(
            decide(&state(Channel::Tag, Relation::Behind), &Config::default()),
            Decision::Hold(reason) if reason == "immutable pin"
        ));
    }

    #[test]
    fn source_metadata_rejects_path_and_option_injection() {
        assert!(valid_segment("herdr-mirror", 100));
        assert!(!valid_segment("../repo", 100));
        assert!(!valid_subdir("../plugin"));
        assert!(!valid_subdir("plugin/../../bad"));
        assert!(valid_ref("release/v1.2"));
        assert!(!valid_ref("-oProxyCommand=bad"));
        assert!(!valid_ref("main..evil"));
    }

    #[test]
    fn a_spent_api_budget_is_reported_as_rate_limited_not_as_no_update() {
        // Exit code matters here: a hold would read as green and a whole fleet
        // would look current the moment it outgrew 60 requests an hour.
        assert!(matches!(
            decide(
                &state(Channel::Branch, Relation::RateLimited),
                &Config::default()
            ),
            Decision::Error(reason) if reason.contains("rate limit")
        ));
    }

    #[test]
    fn rate_limit_wording_covers_every_shape_github_uses() {
        assert!(mentions_rate_limit("API rate limit exceeded for 1.2.3.4"));
        assert!(mentions_rate_limit(
            "You have exceeded a secondary rate limit"
        ));
        assert!(!mentions_rate_limit("Not Found"));
    }

    #[test]
    fn a_status_suffixed_response_splits_into_body_and_code() {
        assert_eq!(
            split_status("{\"status\":\"ahead\"}\n200"),
            ("{\"status\":\"ahead\"}", Some(200))
        );
        assert_eq!(split_status("403").1, Some(403));
        assert_eq!(split_status("no status here").1, None);
    }

    #[test]
    fn github_compare_direction_is_mapped_from_the_remote_head() {
        assert_eq!(relation_from_status("ahead"), Some(Relation::Behind));
        assert_eq!(relation_from_status("behind"), Some(Relation::Ahead));
    }
}
