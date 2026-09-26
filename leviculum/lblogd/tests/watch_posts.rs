//! The filesystem watcher against a real directory and real inotify events.
//!
//! The debounce rule itself is unit-tested with paused time; what this covers
//! is the part that cannot be faked: that notify actually reports changes in
//! the posts directory and that they end in a published snapshot.
//!
//! Waiting is done by polling for the expected state with a generous deadline
//! rather than by sleeping for a fixed span, so the tests neither race the
//! watcher nor depend on how fast the filesystem reports.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lblogd::content::{Reloader, SnapshotRx, Sources};
use lblogd::files::FileArea;
use lblogd::render::BlogMeta;
use lblogd::watcher::PostsWatcher;

/// Generous upper bound: the watcher must settle for QUIET_PERIOD (500 ms)
/// before it reloads at all, so anything under a second proves nothing.
const DEADLINE: Duration = Duration::from_secs(10);

/// Start a watcher over `posts_dir` in the background.
///
/// `PostsWatcher::start` runs before the spawn on purpose: it is what makes
/// the watch active on return, so a change made by the caller immediately
/// afterwards is queued rather than lost to the runtime not having polled the
/// task yet.
fn spawn_watcher(posts_dir: &Path) -> SnapshotRx {
    let (reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir)).expect("initial load");
    let watcher = PostsWatcher::start(&[posts_dir], &[]).expect("establish watch");
    tokio::spawn(watcher.run(Arc::new(reloader)));
    content
}

/// Poll the snapshot until `want` holds, or fail with what it last saw.
async fn wait_until(content: &SnapshotRx, what: &str, want: impl Fn(&[String]) -> bool) {
    let started = Instant::now();
    let mut last = Vec::new();
    while started.elapsed() < DEADLINE {
        last = content.borrow().served_paths();
        if want(&last) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}; snapshot still holds {last:?}");
}

#[tokio::test]
async fn a_new_file_is_picked_up_without_a_signal() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(posts_dir.path().join("erster.md"), "Erster Text.\n").expect("write first");
    let content = spawn_watcher(posts_dir.path());
    assert_eq!(
        content.borrow().served_paths(),
        vec!["/page/erster.mu", "/page/index.mu"]
    );

    std::fs::write(posts_dir.path().join("zweiter.md"), "Zweiter Text.\n").expect("write second");

    wait_until(&content, "the new post to appear", |paths| {
        paths.iter().any(|p| p == "/page/zweiter.mu")
    })
    .await;
}

#[tokio::test]
async fn a_reload_does_not_retrigger_the_watcher() {
    // inotify reports opening the directory, and every reload opens it to
    // read the posts. If that fed back in, one change would drive an endless
    // reload loop at the debounce interval, forever.
    let posts_dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(posts_dir.path().join("erster.md"), "Erster Text.\n").expect("write first");
    let mut content = spawn_watcher(posts_dir.path());
    content.borrow_and_update();

    std::fs::write(posts_dir.path().join("zweiter.md"), "Zweiter Text.\n").expect("write second");
    wait_until(&content, "the new post to appear", |paths| {
        paths.iter().any(|p| p == "/page/zweiter.mu")
    })
    .await;
    content.borrow_and_update();

    // Nothing touches the directory from here on, so any further reload can
    // only have come from the watcher reacting to its own read.
    let mut extra = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if tokio::time::timeout(Duration::from_millis(250), content.changed())
            .await
            .is_ok()
        {
            extra += 1;
            content.borrow_and_update();
        }
    }
    assert_eq!(
        extra, 0,
        "a settled directory must stop reloading, got {extra} further reloads"
    );
}

#[tokio::test]
async fn a_deleted_file_is_picked_up_without_a_signal() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(posts_dir.path().join("erster.md"), "Erster Text.\n").expect("write first");
    std::fs::write(posts_dir.path().join("zweiter.md"), "Zweiter Text.\n").expect("write second");
    let content = spawn_watcher(posts_dir.path());

    std::fs::remove_file(posts_dir.path().join("zweiter.md")).expect("remove second");

    wait_until(&content, "the deleted post to disappear", |paths| {
        !paths.iter().any(|p| p == "/page/zweiter.mu")
    })
    .await;
}

#[tokio::test]
async fn an_edit_is_picked_up_and_a_broken_edit_is_not_fatal() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    let path = posts_dir.path().join("post.md");
    std::fs::write(&path, "+++\ntitle = \"Erst so\"\n+++\n\nText.\n").expect("write post");
    let content = spawn_watcher(posts_dir.path());
    assert!(content
        .borrow()
        .served_paths()
        .iter()
        .any(|p| p == "/page/erst-so.mu"));

    // An edit that changes the title changes the slug, and so the page path.
    std::fs::write(&path, "+++\ntitle = \"Dann so\"\n+++\n\nText.\n").expect("rewrite post");
    wait_until(&content, "the retitled post", |paths| {
        paths.iter().any(|p| p == "/page/dann-so.mu")
    })
    .await;

    // Now break it. The watcher fires, the load fails, and the last good
    // snapshot has to stay in place rather than the blog going empty.
    std::fs::write(&path, "+++\ndate = \"2026-13-45\"\n+++\n\nText.\n").expect("break post");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        content
            .borrow()
            .served_paths()
            .iter()
            .any(|p| p == "/page/dann-so.mu"),
        "a failed reload must leave the previous content serving"
    );

    // And recovering the file recovers the blog, with no restart in between.
    std::fs::write(&path, "+++\ntitle = \"Wieder gut\"\n+++\n\nText.\n").expect("fix post");
    wait_until(&content, "the repaired post", |paths| {
        paths.iter().any(|p| p == "/page/wieder-gut.mu")
    })
    .await;
}

#[tokio::test]
async fn a_picture_added_to_the_file_area_is_picked_up_without_a_signal() {
    // The file area is watched like the posts directory: dropping a photograph
    // next to the posts publishes it, on both sides, with no signal and no
    // restart.
    let posts_dir = tempfile::tempdir().expect("posts dir");
    let files_dir = tempfile::tempdir().expect("files dir");
    std::fs::write(posts_dir.path().join("erster.md"), "Erster Text.\n").expect("write first");

    let sources = Sources::new(posts_dir.path()).with_files(Some(FileArea::new(files_dir.path())));
    let (reloader, content) = Reloader::new(fixture_meta(), sources).expect("initial load");
    let watcher = PostsWatcher::start_for(reloader.sources()).expect("establish watch");
    tokio::spawn(watcher.run(Arc::new(reloader)));
    assert!(content.borrow().served_file_paths().is_empty());

    std::fs::write(files_dir.path().join("antenne.png"), b"pixels").expect("write picture");
    wait_for(&content, "the new picture to appear", |snapshot| {
        snapshot
            .served_file_paths()
            .iter()
            .any(|p| p == "/file/antenne.png")
    })
    .await;

    std::fs::remove_file(files_dir.path().join("antenne.png")).expect("remove picture");
    wait_for(&content, "the deleted picture to disappear", |snapshot| {
        snapshot.served_file_paths().is_empty()
    })
    .await;
}

/// Poll the snapshot itself until `want` holds. The sibling [`wait_until`]
/// looks only at page paths, which is the wrong surface for the file area.
async fn wait_for(
    content: &SnapshotRx,
    what: &str,
    want: impl Fn(&lblogd::content::Snapshot) -> bool,
) {
    let started = Instant::now();
    while started.elapsed() < DEADLINE {
        if want(&content.borrow()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Blog metadata for these fixtures.
fn fixture_meta() -> BlogMeta {
    BlogMeta {
        title: "Test Blog".to_string(),
        language: "en".to_string(),
        ..BlogMeta::default()
    }
}
