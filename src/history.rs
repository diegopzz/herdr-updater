use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    PluginUpdated,
    PluginRolledBack,
    PluginResumed,
    CoreUpdated,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    pub unix_seconds: u64,
    pub kind: EventKind,
    pub target: String,
    pub previous: String,
    pub current: String,
    #[serde(default)]
    pub tracking_ref: Option<String>,
}

impl Event {
    pub fn new(
        kind: EventKind,
        target: impl Into<String>,
        previous: impl Into<String>,
        current: impl Into<String>,
        tracking_ref: Option<String>,
    ) -> Self {
        Self {
            unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            kind,
            target: target.into(),
            previous: previous.into(),
            current: current.into(),
            tracking_ref,
        }
    }
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("state.jsonl")
}

pub fn append(path: &Path, event: &Event) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot append {}: {e}", path.display()))?;
    serde_json::to_writer(&mut file, event)
        .map_err(|e| format!("cannot encode history event: {e}"))?;
    file.write_all(b"\n")
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_data())
        .map_err(|e| format!("cannot persist {}: {e}", path.display()))
}

pub fn read(path: &Path) -> Result<Vec<Event>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            other => Some((index, other)),
        })
        .map(|(index, line)| {
            let line = line.map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
            serde_json::from_str(&line)
                .map_err(|e| format!("{}:{}: invalid JSON: {e}", path.display(), index + 1))
        })
        .collect()
}

pub fn latest_plugins(events: &[Event]) -> BTreeMap<String, Event> {
    let mut latest = BTreeMap::new();
    for event in events {
        if event.kind != EventKind::CoreUpdated {
            latest.insert(event.target.clone(), event.clone());
        }
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_event_per_plugin_drives_rollback_state() {
        let events = vec![
            Event::new(EventKind::PluginUpdated, "a", "1", "2", Some("main".into())),
            Event::new(EventKind::PluginUpdated, "b", "3", "4", None),
            Event::new(
                EventKind::PluginRolledBack,
                "a",
                "2",
                "1",
                Some("main".into()),
            ),
        ];
        let latest = latest_plugins(&events);
        assert_eq!(latest["a"].kind, EventKind::PluginRolledBack);
        assert_eq!(latest["b"].kind, EventKind::PluginUpdated);
    }
}
