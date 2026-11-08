use crate::filter::PathFilter;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

pub enum WatchEvent {
    Changed(PathBuf),
    Error(String),
}

/// Recursive notify watcher. Keeping this struct alive keeps the OS watch alive.
pub struct RepoWatcher {
    _watcher: RecommendedWatcher,
    pub events: Receiver<WatchEvent>,
}

impl RepoWatcher {
    pub fn new(root: &Path, excluded: &[String]) -> Result<Self, String> {
        let filter = PathFilter::new(root, excluded);
        let (tx, rx) = channel::<WatchEvent>();

        let mut watcher = RecommendedWatcher::new(
            move |result: Result<notify::Event, notify::Error>| match result {
                Ok(event) => {
                    // Access/Other events are noise (reads, attribute probes).
                    if matches!(&event.kind, EventKind::Access(_) | EventKind::Other) {
                        return;
                    }
                    for path in event.paths {
                        if !filter.is_excluded(&path) {
                            let _ = tx.send(WatchEvent::Changed(path));
                        }
                    }
                }
                Err(error) => {
                    // An overflow means changes were dropped: treat it as a change.
                    let _ = tx.send(WatchEvent::Error(error.to_string()));
                }
            },
            Config::default(),
        )
        .map_err(|error| error.to_string())?;

        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|error| error.to_string())?;

        Ok(Self {
            _watcher: watcher,
            events: rx,
        })
    }
}
