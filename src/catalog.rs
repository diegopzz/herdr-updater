use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{exec, herdr, plugins};

const MARKETPLACE_URL: &str = "https://assets.herdr.dev/plugins/index.json";
const CACHE_FILE: &str = "marketplace-v1.json";
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPOSITORIES: usize = 5_000;
const MAX_MANIFESTS_PER_REPOSITORY: usize = 100;
const MAX_RESULTS: usize = 500;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    schema_version: u32,
    generated_at: String,
    plugins: Vec<Repository>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    full_name: String,
    owner: String,
    name: String,
    description: Option<String>,
    url: String,
    stars: u64,
    language: Option<String>,
    pushed_at: Option<String>,
    head_commit: String,
    #[serde(default)]
    stars_delta_7d: Option<i64>,
    manifests: Vec<Manifest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    path: String,
    id: String,
    name: String,
    version: String,
    min_herdr_version: String,
    description: Option<String>,
    platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogItem {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub min_herdr_version: String,
    pub description: Option<String>,
    pub platforms: Option<Vec<String>>,
    pub source: String,
    pub repository: String,
    pub repository_url: String,
    pub head_commit: String,
    pub stars: u64,
    pub stars_delta_7d: Option<i64>,
    pub language: Option<String>,
    pub pushed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Relevance,
    Stars,
    Trending,
    Recent,
    Name,
}

pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub json: bool,
    pub sort: SortMode,
    pub limit: usize,
    pub refresh: bool,
}

impl SortMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "relevance" => Ok(Self::Relevance),
            "stars" | "popular" => Ok(Self::Stars),
            "trending" => Ok(Self::Trending),
            "recent" | "updated" => Ok(Self::Recent),
            "name" => Ok(Self::Name),
            _ => Err("--sort must be relevance, stars, trending, recent, or name".into()),
        }
    }
}

#[derive(Debug)]
pub struct Catalog {
    pub generated_at: String,
    pub items: Vec<CatalogItem>,
    pub stale: bool,
    pub source: &'static str,
}

pub fn load(
    config_dir: &Path,
    config: &Config,
    timeout: Duration,
    refresh: bool,
) -> Result<Catalog, String> {
    let cache = config_dir.join(CACHE_FILE);
    if !refresh && cache_is_fresh(&cache, config.catalog_refresh_every()) {
        if let Ok(raw) = read_bounded(&cache) {
            if let Ok((generated_at, items)) = parse(&raw) {
                return Ok(Catalog {
                    generated_at,
                    items,
                    stale: false,
                    source: "cache",
                });
            }
        }
    }

    match fetch(timeout).and_then(|raw| {
        let parsed = parse(&raw)?;
        persist_cache(&cache, raw.as_bytes())?;
        Ok(parsed)
    }) {
        Ok((generated_at, items)) => Ok(Catalog {
            generated_at,
            items,
            stale: false,
            source: "network",
        }),
        Err(network_error) => {
            let raw = read_bounded(&cache).map_err(|cache_error| {
                format!(
                    "marketplace unavailable ({network_error}); no usable cache ({cache_error})"
                )
            })?;
            let (generated_at, items) = parse(&raw).map_err(|cache_error| {
                format!(
                    "marketplace unavailable ({network_error}); cache is invalid ({cache_error})"
                )
            })?;
            Ok(Catalog {
                generated_at,
                items,
                stale: true,
                source: "stale-cache",
            })
        }
    }
}

fn fetch(timeout: Duration) -> Result<String, String> {
    if !exec::have("curl") {
        return Err("curl is not available".into());
    }
    let max_time = timeout.as_secs().max(1).to_string();
    let max_bytes = MAX_SNAPSHOT_BYTES.to_string();
    let out = exec::run(
        "curl",
        &[
            "-fsSL",
            "--max-time",
            &max_time,
            "--max-filesize",
            &max_bytes,
            "-H",
            "Accept: application/json",
            MARKETPLACE_URL,
        ],
        timeout,
    )
    .map_err(|error| format!("marketplace request: {error}"))?;
    if !out.ok() {
        return Err(format!(
            "marketplace request exited {}: {}",
            out.code,
            out.stderr.lines().next().unwrap_or("no stderr")
        ));
    }
    if out.stdout.len() > MAX_SNAPSHOT_BYTES {
        return Err("marketplace snapshot exceeds the size limit".into());
    }
    Ok(out.stdout)
}

fn parse(raw: &str) -> Result<(String, Vec<CatalogItem>), String> {
    if raw.len() > MAX_SNAPSHOT_BYTES {
        return Err("marketplace snapshot exceeds the size limit".into());
    }
    let snapshot: Snapshot = serde_json::from_str(raw)
        .map_err(|error| format!("marketplace snapshot is not valid JSON: {error}"))?;
    if snapshot.schema_version != 1 {
        return Err(format!(
            "unsupported marketplace schema {}",
            snapshot.schema_version
        ));
    }
    if snapshot.generated_at.len() > 64 || snapshot.generated_at.trim().is_empty() {
        return Err("marketplace generatedAt is invalid".into());
    }
    if snapshot.plugins.len() > MAX_REPOSITORIES {
        return Err("marketplace contains too many repositories".into());
    }

    let mut items = Vec::new();
    for repository in snapshot.plugins {
        validate_repository(&repository)?;
        if repository.manifests.len() > MAX_MANIFESTS_PER_REPOSITORY {
            return Err(format!(
                "{} contains too many manifests",
                repository.full_name
            ));
        }
        for manifest in repository.manifests {
            validate_manifest(&manifest)?;
            let source = install_source(&repository.owner, &repository.name, &manifest.path)?;
            items.push(CatalogItem {
                plugin_id: manifest.id,
                name: manifest.name,
                version: manifest.version,
                min_herdr_version: manifest.min_herdr_version,
                description: manifest
                    .description
                    .or_else(|| repository.description.clone()),
                platforms: manifest.platforms,
                source,
                repository: repository.full_name.clone(),
                repository_url: repository.url.clone(),
                head_commit: repository.head_commit.clone(),
                stars: repository.stars,
                stars_delta_7d: repository.stars_delta_7d,
                language: repository.language.clone(),
                pushed_at: repository.pushed_at.clone(),
            });
        }
    }
    Ok((snapshot.generated_at, items))
}

fn validate_repository(repository: &Repository) -> Result<(), String> {
    if !plugins::valid_segment(&repository.owner, 100)
        || !plugins::valid_segment(&repository.name, 100)
        || repository.full_name != format!("{}/{}", repository.owner, repository.name)
        || repository.full_name.len() > 201
        || !plugins::full_commit(&repository.head_commit)
    {
        return Err("marketplace repository metadata failed validation".into());
    }
    let expected_url = format!(
        "https://github.com/{}/{}",
        repository.owner, repository.name
    );
    if repository.url != expected_url {
        return Err(format!(
            "{} has a non-canonical repository URL",
            repository.full_name
        ));
    }
    if repository
        .description
        .as_deref()
        .is_some_and(|value| value.len() > 1_000)
        || repository
            .language
            .as_deref()
            .is_some_and(|value| value.len() > 100)
        || repository
            .pushed_at
            .as_deref()
            .is_some_and(|value| value.len() > 64)
    {
        return Err(format!("{} metadata is too large", repository.full_name));
    }
    Ok(())
}

fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.id.is_empty()
        || manifest.id.len() > 120
        || !manifest
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-'))
        || manifest.name.trim().is_empty()
        || manifest.name.len() > 120
        || manifest.version.trim().is_empty()
        || manifest.version.len() > 64
        || parse_version(&manifest.min_herdr_version).is_none()
        || manifest
            .description
            .as_deref()
            .is_some_and(|value| value.len() > 1_000)
    {
        return Err("marketplace manifest metadata failed validation".into());
    }
    if let Some(platforms) = &manifest.platforms {
        if platforms.is_empty()
            || platforms.len() > 3
            || platforms
                .iter()
                .any(|platform| !matches!(platform.as_str(), "linux" | "macos" | "windows"))
        {
            return Err(format!("{} has invalid platforms", manifest.id));
        }
    }
    Ok(())
}

fn install_source(owner: &str, repo: &str, manifest_path: &str) -> Result<String, String> {
    let subdir = if manifest_path == "herdr-plugin.toml" {
        None
    } else {
        manifest_path.strip_suffix("/herdr-plugin.toml")
    };
    let source = match subdir {
        Some(path) if plugins::valid_subdir(path) => format!("{owner}/{repo}/{path}"),
        Some(_) => return Err("marketplace manifest path is invalid".into()),
        None if manifest_path == "herdr-plugin.toml" => format!("{owner}/{repo}"),
        None => return Err("marketplace manifest path is invalid".into()),
    };
    plugins::validate_source(&source)?;
    Ok(source)
}

fn cache_is_fresh(path: &Path, max_age: Duration) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= max_age)
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_SNAPSHOT_BYTES as u64 {
        return Err(format!("{} is not a bounded regular file", path.display()));
    }
    fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn persist_cache(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err("marketplace snapshot exceeds the size limit".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(format!(
                "refusing to replace non-regular {}",
                path.display()
            ));
        }
    }
    fs::write(path, bytes).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

pub fn search<'a>(
    items: &'a [CatalogItem],
    query: &str,
    sort: SortMode,
    limit: usize,
) -> Vec<&'a CatalogItem> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|token| token.to_ascii_lowercase())
        .collect();
    let mut matches: Vec<(&CatalogItem, i32)> = items
        .iter()
        .filter_map(|item| {
            let haystack = format!(
                "{} {} {} {} {}",
                item.plugin_id,
                item.name,
                item.source,
                item.description.as_deref().unwrap_or_default(),
                item.language.as_deref().unwrap_or_default()
            )
            .to_ascii_lowercase();
            tokens
                .iter()
                .all(|token| haystack.contains(token))
                .then(|| {
                    let query = query.trim().to_ascii_lowercase();
                    let id = item.plugin_id.to_ascii_lowercase();
                    let name = item.name.to_ascii_lowercase();
                    let source = item.source.to_ascii_lowercase();
                    let score = if query.is_empty() {
                        0
                    } else if id == query || source == query {
                        400
                    } else if id.starts_with(&query) || name.starts_with(&query) {
                        300
                    } else if id.contains(&query) || name.contains(&query) {
                        200
                    } else {
                        100
                    };
                    (item, score)
                })
        })
        .collect();
    matches.sort_by(|(a, a_score), (b, b_score)| {
        let primary = match sort {
            SortMode::Relevance => b_score.cmp(a_score).then(b.stars.cmp(&a.stars)),
            SortMode::Stars => b.stars.cmp(&a.stars),
            SortMode::Trending => b
                .stars_delta_7d
                .unwrap_or(i64::MIN)
                .cmp(&a.stars_delta_7d.unwrap_or(i64::MIN)),
            SortMode::Recent => b.pushed_at.cmp(&a.pushed_at),
            SortMode::Name => a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase()),
        };
        primary.then_with(|| a.plugin_id.cmp(&b.plugin_id))
    });
    matches
        .into_iter()
        .take(limit.clamp(1, MAX_RESULTS))
        .map(|(item, _)| item)
        .collect()
}

pub fn cmd_search(
    request: SearchRequest<'_>,
    config_dir: &Path,
    config: &Config,
    timeout: Duration,
) -> i32 {
    match load(config_dir, config, timeout, request.refresh) {
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            2
        }
        Ok(catalog) => {
            let results = search(&catalog.items, request.query, request.sort, request.limit);
            if request.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "generated_at": catalog.generated_at,
                        "source": catalog.source,
                        "stale": catalog.stale,
                        "query": request.query,
                        "results": results,
                    }))
                    .unwrap_or_else(|_| "{}".into())
                );
            } else {
                println!(
                    "Herdr plugins — {} result(s){}",
                    results.len(),
                    if catalog.stale { " (stale cache)" } else { "" }
                );
                for item in results {
                    println!(
                        "  {:<32} {:>6} stars  {}",
                        item.plugin_id, item.stars, item.source
                    );
                    if let Some(description) = item.description.as_deref() {
                        println!("    {}", one_line(description, 100));
                    }
                }
            }
            0
        }
    }
}

pub fn resolve<'a>(catalog: &'a Catalog, value: &str) -> Result<&'a CatalogItem, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let exact: Vec<&CatalogItem> = catalog
        .items
        .iter()
        .filter(|item| {
            item.plugin_id.eq_ignore_ascii_case(&normalized)
                || item.source.eq_ignore_ascii_case(&normalized)
        })
        .collect();
    match exact.as_slice() {
        [item] => Ok(*item),
        [] => Err(format!("no marketplace plugin matches {value:?}")),
        _ => Err(format!("marketplace plugin {value:?} is ambiguous")),
    }
}

pub fn cmd_install(
    value: &str,
    yes: bool,
    refresh: bool,
    config_dir: &Path,
    config: &Config,
    herdr_bin: &str,
    timeout: Duration,
) -> i32 {
    let catalog = match load(config_dir, config, timeout, refresh) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    let item = match resolve(&catalog, value) {
        Ok(item) => item,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 1;
        }
    };
    if !supports_current_platform(item) {
        eprintln!(
            "herdr-updater: {} does not declare support for {}",
            item.plugin_id,
            current_platform()
        );
        return 1;
    }
    match plugins::install_source(
        herdr_bin,
        &item.source,
        Some(&item.head_commit),
        yes,
        timeout.max(Duration::from_secs(120)),
    ) {
        Ok(()) => {
            println!("installed {} from {}", item.plugin_id, item.source);
            0
        }
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            1
        }
    }
}

pub fn open_store(herdr_bin: &str, timeout: Duration) -> i32 {
    let entrypoint = if cfg!(target_os = "windows") {
        "store-windows"
    } else {
        "store"
    };
    match exec::run(
        herdr_bin,
        &[
            "plugin",
            "pane",
            "open",
            "--plugin",
            "herdr-updater",
            "--entrypoint",
            entrypoint,
            "--focus",
        ],
        timeout,
    ) {
        Ok(output) if output.ok() => 0,
        Ok(output) => {
            eprintln!(
                "herdr-updater: could not open store pane (exit {}): {}",
                output.code,
                output.stderr.lines().next().unwrap_or("no stderr")
            );
            1
        }
        Err(error) => {
            eprintln!("herdr-updater: could not open store pane: {error}");
            1
        }
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|error| format!("cannot enter raw mode: {error}"))?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)
            .map_err(|error| format!("cannot open store screen: {error}"))?;
        Ok(Self { active: true })
    }

    fn leave(&mut self) -> Result<(), String> {
        if self.active {
            execute!(io::stdout(), Show, LeaveAlternateScreen)
                .map_err(|error| format!("cannot close store screen: {error}"))?;
            terminal::disable_raw_mode()
                .map_err(|error| format!("cannot leave raw mode: {error}"))?;
            self.active = false;
        }
        Ok(())
    }

    fn reenter(&mut self) -> Result<(), String> {
        if !self.active {
            terminal::enable_raw_mode()
                .map_err(|error| format!("cannot re-enter raw mode: {error}"))?;
            execute!(io::stdout(), EnterAlternateScreen, Hide)
                .map_err(|error| format!("cannot restore store screen: {error}"))?;
            self.active = true;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

pub fn cmd_store(config_dir: &Path, config: &Config, herdr_bin: &str, timeout: Duration) -> i32 {
    let mut catalog = match load(config_dir, config, timeout, false) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    let mut terminal = match TerminalGuard::enter() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("herdr-updater: {error}");
            return 2;
        }
    };
    let mut query = String::new();
    let mut selected = 0usize;
    let mut search_mode = false;
    let mut confirm = false;
    let mut message = String::new();
    let mut installed = installed_map(herdr_bin, timeout);
    let herdr_version = herdr::status(herdr_bin, timeout)
        .ok()
        .map(|status| status.client.version);

    loop {
        let results = search(&catalog.items, &query, SortMode::Relevance, MAX_RESULTS);
        if results.is_empty() {
            selected = 0;
        } else {
            selected = selected.min(results.len() - 1);
        }
        if let Err(error) = render_store(
            &catalog,
            &results,
            &installed,
            herdr_version.as_deref(),
            &query,
            selected,
            search_mode,
            confirm,
            &message,
        ) {
            let _ = terminal.leave();
            eprintln!("herdr-updater: {error}");
            return 2;
        }

        let event = match event::read() {
            Ok(event) => event,
            Err(error) => {
                let _ = terminal.leave();
                eprintln!("herdr-updater: cannot read store input: {error}");
                return 2;
            }
        };
        let Event::Key(key) = event else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    confirm = false;
                    let Some(item) = results.get(selected).copied() else {
                        continue;
                    };
                    if !supports_current_platform(item) {
                        message = format!(
                            "{} is not available on {}",
                            item.plugin_id,
                            current_platform()
                        );
                        continue;
                    }
                    if !compatible_with(herdr_version.as_deref(), &item.min_herdr_version) {
                        message = format!(
                            "{} requires Herdr {} or newer",
                            item.plugin_id, item.min_herdr_version
                        );
                        continue;
                    }
                    if let Err(error) = terminal.leave() {
                        eprintln!("herdr-updater: {error}");
                        return 2;
                    }
                    println!(
                        "Installing {} from {} at {}",
                        item.plugin_id,
                        item.source,
                        &item.head_commit[..12]
                    );
                    let result = plugins::install_source(
                        herdr_bin,
                        &item.source,
                        Some(&item.head_commit),
                        true,
                        timeout.max(Duration::from_secs(120)),
                    );
                    if let Err(error) = terminal.reenter() {
                        eprintln!("herdr-updater: {error}");
                        return 2;
                    }
                    match result {
                        Ok(()) => {
                            message = format!("Installed {}", item.plugin_id);
                            installed = installed_map(herdr_bin, timeout);
                        }
                        Err(error) => message = format!("Install failed: {error}"),
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => confirm = false,
                _ => {}
            }
            continue;
        }
        if search_mode {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => search_mode = false,
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(event::KeyModifiers::CONTROL)
                        && !character.is_control()
                        && query.len() < 200 =>
                {
                    query.push(character);
                    selected = 0;
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('/') => search_mode = true,
            KeyCode::Char('c') => {
                query.clear();
                selected = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if selected + 1 < results.len() {
                    selected += 1;
                }
            }
            KeyCode::PageUp => selected = selected.saturating_sub(10),
            KeyCode::PageDown => {
                selected = (selected + 10).min(results.len().saturating_sub(1));
            }
            KeyCode::Char('i') | KeyCode::Enter if !results.is_empty() => {
                confirm = true;
                message.clear();
            }
            KeyCode::Char('r') => match load(config_dir, config, timeout, true) {
                Ok(next) => {
                    catalog = next;
                    message = "Marketplace refreshed".into();
                }
                Err(error) => message = format!("Refresh failed: {error}"),
            },
            _ => {}
        }
    }
    0
}

#[allow(clippy::too_many_arguments)]
fn render_store(
    catalog: &Catalog,
    results: &[&CatalogItem],
    installed: &BTreeMap<String, String>,
    herdr_version: Option<&str>,
    query: &str,
    selected: usize,
    search_mode: bool,
    confirm: bool,
    message: &str,
) -> Result<(), String> {
    let (width, height) = terminal::size().unwrap_or((100, 30));
    let width = width.max(40) as usize;
    let height = height.max(14) as usize;
    let list_height = height.saturating_sub(10).max(3);
    let offset = if selected >= list_height {
        selected + 1 - list_height
    } else {
        0
    };
    let mut out = io::stdout();
    execute!(out, MoveTo(0, 0), Clear(ClearType::All))
        .map_err(|error| format!("cannot render store: {error}"))?;
    write_line(&mut out, "HERDR PLUGIN STORE", width, true)?;
    write_line(
        &mut out,
        &format!(
            "/ search  arrows navigate  Enter/i install  r refresh  c clear  q close    {}{}",
            catalog.generated_at,
            if catalog.stale { "  STALE" } else { "" }
        ),
        width,
        false,
    )?;
    write_line(
        &mut out,
        &format!(
            "Search{}: {}",
            if search_mode { " [typing]" } else { "" },
            query
        ),
        width,
        false,
    )?;
    write_line(
        &mut out,
        &format!("{} plugin(s)", results.len()),
        width,
        false,
    )?;

    for row in 0..list_height {
        let index = offset + row;
        if let Some(item) = results.get(index) {
            let installed_mark = installed
                .get(&item.plugin_id)
                .map(|kind| {
                    if kind == "local" {
                        "linked"
                    } else {
                        "installed"
                    }
                })
                .unwrap_or("");
            let compatibility = if !supports_current_platform(item) {
                "unsupported"
            } else if !compatible_with(herdr_version, &item.min_herdr_version) {
                "needs newer Herdr"
            } else {
                installed_mark
            };
            let line = format!(
                "{:<34} {:>6} stars  {:<17} {}",
                one_line(&item.name, 34),
                item.stars,
                compatibility,
                item.source
            );
            if index == selected {
                execute!(out, SetAttribute(Attribute::Reverse))
                    .map_err(|error| format!("cannot render selection: {error}"))?;
                write_line(&mut out, &line, width, false)?;
                execute!(out, SetAttribute(Attribute::Reset))
                    .map_err(|error| format!("cannot render selection: {error}"))?;
            } else {
                write_line(&mut out, &line, width, false)?;
            }
        } else {
            write_line(&mut out, "", width, false)?;
        }
    }

    if let Some(item) = results.get(selected) {
        write_line(
            &mut out,
            &format!(
                "{}  v{}  requires Herdr {}",
                item.plugin_id, item.version, item.min_herdr_version
            ),
            width,
            true,
        )?;
        write_line(
            &mut out,
            item.description.as_deref().unwrap_or("No description"),
            width,
            false,
        )?;
        write_line(
            &mut out,
            &format!("Source: {} @ {}", item.source, &item.head_commit[..12]),
            width,
            false,
        )?;
    } else {
        write_line(&mut out, "No matching plugins", width, true)?;
        write_line(&mut out, "", width, false)?;
        write_line(&mut out, "", width, false)?;
    }
    let footer = if confirm {
        results
            .get(selected)
            .map(|item| format!("Install {} at the indexed commit? y/N", item.plugin_id))
            .unwrap_or_default()
    } else {
        message.to_string()
    };
    write_line(&mut out, &footer, width, true)?;
    out.flush()
        .map_err(|error| format!("cannot flush store screen: {error}"))
}

fn write_line(out: &mut io::Stdout, value: &str, width: usize, bold: bool) -> Result<(), String> {
    if bold {
        execute!(out, SetAttribute(Attribute::Bold))
            .map_err(|error| format!("cannot render store: {error}"))?;
    }
    let line = one_line(value, width.saturating_sub(1));
    execute!(
        out,
        Print(format!(
            "{line:<width$}\r\n",
            width = width.saturating_sub(1)
        ))
    )
    .map_err(|error| format!("cannot render store: {error}"))?;
    if bold {
        execute!(out, SetAttribute(Attribute::Reset))
            .map_err(|error| format!("cannot render store: {error}"))?;
    }
    Ok(())
}

fn installed_map(herdr_bin: &str, timeout: Duration) -> BTreeMap<String, String> {
    plugins::list_installed(herdr_bin, timeout)
        .unwrap_or_default()
        .into_iter()
        .map(|plugin| {
            let kind = plugin.source.map(|source| source.kind).unwrap_or_default();
            (plugin.plugin_id, kind)
        })
        .collect()
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

fn supports_current_platform(item: &CatalogItem) -> bool {
    item.platforms.as_ref().map_or(true, |platforms| {
        platforms
            .iter()
            .any(|platform| platform == current_platform())
    })
}

fn compatible_with(current: Option<&str>, minimum: &str) -> bool {
    let Some(minimum) = parse_version(minimum) else {
        return false;
    };
    let Some(current) = current.and_then(parse_version) else {
        return true;
    };
    current >= minimum
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let core = value.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

fn one_line(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        normalized
    } else if max_chars == 0 {
        String::new()
    } else {
        let mut value: String = normalized
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        value.push('…');
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "schemaVersion": 1,
      "generatedAt": "2026-08-30T00:00:00Z",
      "plugins": [{
        "fullName": "diegopzz/herdr-tools",
        "owner": "diegopzz",
        "name": "herdr-tools",
        "description": "Useful terminal tools",
        "url": "https://github.com/diegopzz/herdr-tools",
        "stars": 12,
        "language": "Rust",
        "pushedAt": "2026-08-30T00:00:00Z",
        "headCommit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "starsDelta7d": 3,
        "manifests": [{
          "path": "plugins/finder/herdr-plugin.toml",
          "id": "tools.finder",
          "name": "Finder",
          "version": "1.0.0",
          "minHerdrVersion": "0.8.2",
          "description": "Find plugins",
          "platforms": ["linux", "macos", "windows"]
        }]
      }]
    }"#;

    #[test]
    fn validates_and_flattens_marketplace_items() {
        let (_, items) = parse(FIXTURE).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source, "diegopzz/herdr-tools/plugins/finder");
        assert_eq!(items[0].head_commit, "a".repeat(40));
    }

    #[test]
    fn search_prefers_an_exact_plugin_id() {
        let (_, mut items) = parse(FIXTURE).unwrap();
        let mut second = items[0].clone();
        second.plugin_id = "other.finder".into();
        second.name = "Other Finder".into();
        second.stars = 1000;
        items.push(second);
        let matches = search(&items, "tools.finder", SortMode::Relevance, 10);
        assert_eq!(matches[0].plugin_id, "tools.finder");
    }

    #[test]
    fn rejects_a_manifest_path_that_could_escape_the_source() {
        assert!(install_source("diegopzz", "repo", "../herdr-plugin.toml").is_err());
    }

    #[test]
    fn semantic_versions_are_compared_numerically() {
        assert!(compatible_with(Some("0.10.0"), "0.8.2"));
        assert!(!compatible_with(Some("0.7.9"), "0.8.2"));
    }
}
