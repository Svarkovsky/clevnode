//! Optional filesystem watcher: reload the posts when the directory changes,
//! without waiting for a SIGHUP.
//!
//! This is a trigger, not a second reload path. It ends in the same
//! [`Reloader::reload`] the signal handler calls, with the same guarantee
//! that a failed load changes nothing.
//!
//! Debouncing is the whole difficulty. Editors do not write a file once: they
//! truncate and append in stages, write a temporary file and rename it over
//! the target, or drop editor swap files next to it. Reloading on every raw
//! event would repeatedly parse half-written files and log errors for
//! problems that resolve themselves milliseconds later. So the watcher waits
//! for the directory to go quiet for [`QUIET_PERIOD`] and then reloads once,
//! however many events the burst contained.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::event::EventKind;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::content::{Reloader, Sources};

/// How long the directory must be quiet before a burst counts as finished.
///
/// Long enough to cover an editor's multi-step save, short enough that
/// publishing still feels immediate.
pub const QUIET_PERIOD: Duration = Duration::from_millis(500);

/// How many raw events may queue before the watcher starts dropping them.
///
/// Dropping is harmless here: an event carries no information beyond "the
/// directory changed", and the reload reads the whole directory anyway. A
/// full channel therefore means a reload is already coming.
const EVENT_QUEUE: usize = 64;

/// Errors from setting up the watcher.
#[derive(Debug, Error)]
pub enum WatchError {
    /// The watcher could not be created or could not watch the directory,
    /// e.g. the inotify watch limit is exhausted.
    #[error("watching {path}: {source}")]
    Watch {
        /// The directory that could not be watched.
        path: String,
        /// The underlying notify error.
        source: notify::Error,
    },
}

/// An established watch on the posts directory, ready to be driven.
///
/// Setting the watch up is deliberately **synchronous** and separate from
/// running it. Were it done inside the async task, the watch would only be
/// established once the runtime first polled that task, and every change made
/// before that moment would be missed silently. Splitting it means that once
/// `start` has returned, changes are being recorded, even if nothing is
/// consuming them yet: they queue until [`run`](Self::run) is polled.
pub struct PostsWatcher {
    events: mpsc::Receiver<()>,
    /// Held only to keep the watch alive: notify stops delivering the moment
    /// the watcher is dropped.
    _watcher: RecommendedWatcher,
}

impl PostsWatcher {
    /// Establish the watch on everything a snapshot is built from: the posts
    /// directory, the file area, the stylesheet and the about text.
    pub fn start_for(sources: &Sources) -> Result<PostsWatcher, WatchError> {
        let mut dirs: Vec<&Path> = vec![sources.posts_dir.as_path()];
        if let Some(files) = &sources.files {
            dirs.push(files.dir.as_path());
        }
        let single: Vec<&Path> = [sources.css_path.as_deref(), sources.about_path.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        PostsWatcher::start(&dirs, &single)
    }

    /// Establish the watch on the content directories and files. Changes are
    /// recorded from here on.
    ///
    /// `dirs` are directories every change inside counts (the posts directory
    /// and the file area); `files` names individual files outside them that
    /// also belong to the content: the stylesheet and the about text.
    ///
    /// The individual files are watched through their parent directory rather
    /// than directly: an inotify watch follows the inode, and editors that
    /// save by writing a temporary file and renaming it over the target would
    /// leave a direct watch pointing at the replaced file. Watching the
    /// directory and filtering by path survives that. It does mean events for
    /// their neighbours arrive too; the watcher discards those by path before
    /// recording a change.
    ///
    /// A directory that does not exist is skipped with a note rather than
    /// failing the run: the file area is optional by existence, and a blog
    /// with no pictures should still start.
    pub fn start(dirs: &[&Path], files: &[&Path]) -> Result<PostsWatcher, WatchError> {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        let watch_err = |path: &Path| {
            let path = path.display().to_string();
            move |source| WatchError::Watch {
                path: path.clone(),
                source,
            }
        };

        let targets = Targets {
            dirs: dirs.iter().map(|p| p.to_path_buf()).collect(),
            files: files.iter().map(|p| p.to_path_buf()).collect(),
        };
        let first = dirs.first().copied().unwrap_or(Path::new("."));
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                // Which file changed matters only insofar as it has to be one
                // of ours; the reload re-reads everything either way.
                // try_send drops when the queue is full, which is correct: a
                // reload is already pending.
                if event.is_ok_and(|e| is_content_change(&e.kind) && targets.is_relevant(&e.paths))
                {
                    let _ = tx.try_send(());
                }
            })
            .map_err(watch_err(first))?;

        // Both content directories are flat (the loaders read their top level
        // only), so there is nothing below them worth watching. One watch per
        // distinct directory: watching the same one twice is at best redundant
        // and at worst an error.
        let mut watched: Vec<&Path> = Vec::new();
        for dir in dirs
            .iter()
            .copied()
            .chain(files.iter().filter_map(|f| f.parent()))
        {
            if watched.contains(&dir) {
                continue;
            }
            if !dir.is_dir() {
                eprintln!("lblogd: not watching {}: no such directory", dir.display());
                continue;
            }
            watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(watch_err(dir))?;
            watched.push(dir);
        }

        Ok(PostsWatcher {
            events: rx,
            _watcher: watcher,
        })
    }

    /// Reload on every settled change, forever.
    ///
    /// Returns only when the watch stops producing events, which happens when
    /// this value is dropped.
    pub async fn run(self, reloader: Arc<Reloader>) {
        debounce(self.events, QUIET_PERIOD, || match reloader.reload() {
            Ok(count) => eprintln!("lblogd: posts changed: reloaded {count} posts"),
            Err(e) => {
                eprintln!("lblogd: posts changed: reload failed, keeping previous content: {e}")
            }
        })
        .await;
    }
}

/// What this watcher considers its own, used to discard events for unrelated
/// neighbours of the stylesheet.
struct Targets {
    /// Directories every change inside counts: the posts directory and the
    /// file area.
    dirs: Vec<PathBuf>,
    /// Individual files outside them: the stylesheet and the about text.
    files: Vec<PathBuf>,
}

impl Targets {
    /// Whether any of an event's paths is content we serve.
    ///
    /// A path counts when it sits directly in one of the content directories
    /// or is one of the named files. Watching the stylesheet's directory means
    /// events arrive for every file beside it, and a config file being edited
    /// next to it is no reason to re-render the blog.
    fn is_relevant(&self, paths: &[PathBuf]) -> bool {
        // An event with no paths carries no way to rule it out, and dropping
        // it could lose a change; reloading spuriously only costs a read.
        paths.is_empty()
            || paths.iter().any(|path| {
                path.parent()
                    .is_some_and(|parent| self.dirs.iter().any(|dir| dir == parent))
                    || self.files.contains(path)
            })
    }
}

/// Whether an event means the posts actually changed, as opposed to merely
/// having been looked at.
///
/// This is what keeps the watcher from driving itself: reloading opens the
/// posts directory and reads every file in it, and inotify reports those
/// reads too. Acting on them would make each reload trigger the next, one per
/// debounce interval, for as long as the process ran. Reads surface as
/// [`EventKind::Access`], so ignoring that kind breaks the loop at its source
/// while leaving every genuine change (create, write, rename, delete)
/// covered. Access(Close(Write)) is ignored along with the rest, but the
/// write it follows has already been reported as a `Modify`.
fn is_content_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
        // Reads, opens and closes: the self-triggering case.
        EventKind::Access(_) => false,
        // Unclassified events are rare and backend-specific. Reloading on one
        // costs a directory read; ignoring one would lose a post silently.
        EventKind::Any | EventKind::Other => true,
    }
}

/// Call `on_settled` once per burst of events, after `quiet` has passed with
/// no further event.
///
/// Split out from the filesystem so the collapsing rule can be tested without
/// an editor, a real directory, or wall-clock waits.
async fn debounce<F: FnMut()>(mut events: mpsc::Receiver<()>, quiet: Duration, mut on_settled: F) {
    while events.recv().await.is_some() {
        loop {
            match tokio::time::timeout(quiet, events.recv()).await {
                // More activity: the burst is still going, keep waiting.
                Ok(Some(())) => continue,
                // The sender is gone mid-burst. Something did change, so run
                // once more before giving up rather than dropping it.
                Ok(None) => {
                    on_settled();
                    return;
                }
                // Quiet for long enough: the burst is over.
                Err(_) => break,
            }
        }
        on_settled();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Run `debounce` over a script of sends and return how often it fired.
    ///
    /// Paused time makes this deterministic: tokio advances the clock only
    /// when every task is idle, so "quiet elapsed" happens exactly when the
    /// script stops sending, with no wall-clock waiting and no flakiness.
    async fn fire_count(script: impl FnOnce(mpsc::Sender<()>)) -> usize {
        let (tx, rx) = mpsc::channel(64);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            debounce(rx, Duration::from_millis(500), || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .await;
        });
        script(tx);
        task.await.unwrap();
        calls.load(Ordering::SeqCst)
    }

    #[test]
    fn reads_are_not_changes_but_everything_else_is() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind,
        };

        // The self-triggering kinds: exactly what reloading the directory
        // produces by opening and reading it.
        assert!(!is_content_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));

        // The kinds a real edit produces.
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
        assert!(is_content_change(&EventKind::Any));
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_events_reloads_once() {
        // The editor-save case: one logical change, many raw events.
        let fired = fire_count(|tx| {
            for _ in 0..10 {
                tx.try_send(()).unwrap();
            }
        })
        .await;
        assert_eq!(fired, 1, "a burst must collapse into one reload");
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_event_reloads_once() {
        let fired = fire_count(|tx| tx.try_send(()).unwrap()).await;
        assert_eq!(fired, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn changes_separated_by_quiet_reload_separately() {
        let (tx, rx) = mpsc::channel(64);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            debounce(rx, Duration::from_millis(500), || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .await;
        });

        tx.send(()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "first change fired");

        tx.send(()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a later change is its own reload, not part of the first"
        );

        drop(tx);
        task.await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "closing after a settled burst must not fire again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_events_never_reloads() {
        let fired = fire_count(drop).await;
        assert_eq!(fired, 0, "an idle directory must not reload");
    }
}
