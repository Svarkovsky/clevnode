//! The honest-metric test: what the mesh side can and cannot count, proved
//! over a real link rather than over a mock.
//!
//! The claim under test is the one the counter's field names make: a `LinkId`
//! is a session, not a person, and requests are requests. One reader fetching
//! two pages is therefore **one session and two requests** — and if the two
//! numbers were ever allowed to drift into meaning the same thing, one of
//! them would be a visitor count wearing a different name.
//!
//! Topology is `node_integ.rs`'s, and for the same reason: a shared-instance
//! daemon with the blog node as a client and `lnomad`'s real fetch path as
//! the reader, all in one process. `NodeEvent::LinkEstablished` arriving once
//! per link, on the client side of the IPC, is an assumption about the stack
//! that only this shape can check.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use lblogd::content::{Reloader, Sources};
use lblogd::counter::Counter;
use lblogd::node::{BlogNode, BlogNodeConfig};
use lblogd::render::BlogMeta;
use lnomad::fetch::Session;
use lnomad::url::parse_url;

#[tokio::test]
async fn two_requests_on_one_link_are_one_session_and_two_requests() {
    // `:0`: the kernel assigns the port at bind and nothing dials this
    // server; the test wires everything over the shared instance
    // (Codeberg #221).
    let daemon_tcp: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let instance_name = format!("lblogd-counter-test-{}", std::process::id());
    let daemon_storage = tempfile::tempdir().expect("daemon storage");
    let mut daemon = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp)
        .storage_path(daemon_storage.path().to_path_buf())
        .build_sync()
        .expect("build daemon");
    daemon.start().await.expect("start daemon");
    // Let the abstract Unix socket listener come up before clients connect.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let posts_dir = tempfile::tempdir().expect("posts dir");
    std::fs::write(
        posts_dir.path().join("hello.md"),
        "+++\ntitle = \"Hello Mesh\"\ndate = \"2026-07-01\"\nslug = \"hello\"\n+++\n\nA post.\n",
    )
    .expect("write hello.md");
    let data_dir = tempfile::tempdir().expect("data dir");
    let counts_dir = tempfile::tempdir().expect("counter dir");
    let counts_path = counts_dir.path().join("counts.log");

    let (_reloader, content) = Reloader::new(
        BlogMeta {
            title: "lblogd counter test".to_string(),
            language: "en".to_string(),
            ..BlogMeta::default()
        },
        Sources::new(posts_dir.path()),
    )
    .expect("initial content load");

    let counter = Arc::new(Counter::open(&counts_path).expect("open counter"));
    let blog = BlogNode::start(
        BlogNodeConfig {
            instance_name: instance_name.clone(),
            data_dir: data_dir.path().to_path_buf(),
            display_name: "lblogd counter test".to_string(),
            announce_interval: Duration::from_secs(3600),
        },
        content,
    )
    .await
    .expect("start blog node")
    .with_counter(Arc::clone(&counter));
    let dest_hex = hex::encode(blog.destination_hash().as_bytes());
    let blog_task = tokio::spawn(blog.run());
    tokio::time::sleep(Duration::from_millis(500)).await;

    // One reader. One session object, so one link, and two pages fetched
    // over it.
    let session_storage = tempfile::tempdir().expect("session storage");
    let app_dir = tempfile::tempdir().expect("session app dir");
    let mut session = Session::connect_to_with_app_dir(
        &instance_name,
        session_storage.path().to_path_buf(),
        Some(app_dir.path().to_path_buf()),
    )
    .await
    .expect("lnomad session connect");

    for path in ["/page/index.mu", "/page/hello.mu"] {
        let target = parse_url(&format!("{dest_hex}:{path}"), None).expect("parse url");
        session
            .fetch(&target, Duration::from_secs(20))
            .await
            .unwrap_or_else(|e| panic!("fetch {path}: {e:?}"));
    }
    // The node counts on its select! loop; give the second request's event a
    // turn to be handled before reading the totals.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (_, counts) = counter.open_day();
    assert_eq!(
        counts.mesh_requests, 2,
        "two page fetches are two requests, not one reader"
    );
    assert_eq!(
        counts.mesh_sessions, 1,
        "and they arrived over one link, which is one session — a second \
         session here would mean the number is counting requests twice under \
         another name"
    );
    assert_eq!(
        counts.mesh_identified_requests, 0,
        "nothing about fetching a public page asks a reader to identify, so \
         the identified count must be an observed zero rather than an assumed \
         one"
    );

    // And the file says the same thing, in the shape a reader parses.
    counter.flush().expect("flush");
    let text = std::fs::read_to_string(&counts_path).expect("read counts file");
    let record = text
        .lines()
        .rfind(|line| line.starts_with("DAY "))
        .expect("a record for today");
    assert!(record.contains(" mesh_requests=2 "), "{record}");
    assert!(record.contains(" mesh_sessions=1 "), "{record}");
    assert!(record.contains(" tz=UTC "), "{record}");

    session.close().await.expect("close session");
    blog_task.abort();
    daemon.stop().await.expect("stop daemon");
}
