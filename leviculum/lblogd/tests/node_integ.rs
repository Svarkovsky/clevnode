//! End-to-end test of the lblogd NomadNet page node over a real
//! shared-instance IPC path, with no Python and no hardware.
//!
//! Topology (the production shape, all in one process):
//! ```text
//! lnomad Session ── Unix socket ── Rust daemon ── Unix socket ── BlogNode
//! (the browser's                   (share_instance,              (shared-instance
//!  fetch path)                      TCP server)                   client, page node)
//! ```
//!
//! The assertions prove the served content is byte-exactly the renderer
//! output through the real fetch path: the index page and a small post over
//! the single-packet RESPONSE path, a post above the 262144 byte link MDU
//! over the `send_response_resource` fallback, and a clean client timeout
//! for an unknown path.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use lblogd::content::{Reloader, Sources};
use lblogd::files::FileArea;
use lblogd::node::{BlogNode, BlogNodeConfig};
use lblogd::post::load_posts_dir;
use lblogd::render::{render_index_micron, render_post_micron, BlogMeta};
use lnomad::fetch::{FetchError, Session};
use lnomad::url::parse_url;

/// Write the fixture posts: two small ones and one whose body pushes the
/// rendered Micron well past the 262144 byte negotiated TCP/IPC link MDU, so
/// serving it forces the response Resource path.
fn write_fixture_posts(dir: &Path) {
    std::fs::write(
        dir.join("hello.md"),
        "+++\ntitle = \"Hello Mesh\"\ndate = \"2026-07-01\"\n+++\n\nFirst post, *small* enough for one packet.\n",
    )
    .expect("write hello.md");
    std::fs::write(
        dir.join("second.md"),
        "+++\ntitle = \"Second Post\"\ndate = \"2026-07-05\"\nslug = \"second\"\n+++\n\nAnother small post.\n",
    )
    .expect("write second.md");

    let mut large = String::from(
        "+++\ntitle = \"Large Post\"\ndate = \"2026-07-10\"\nslug = \"large\"\n+++\n\n",
    );
    for i in 0..9000 {
        large.push_str(&format!("Line {i:04} of the large fixture post body.\n"));
    }
    std::fs::write(dir.join("large.md"), large).expect("write large.md");
}

#[tokio::test]
async fn blog_node_serves_pages_end_to_end() {
    // The daemon: a transport node sharing its instance over IPC, standing in
    // for a production lnsd.
    // `:0`: the kernel assigns the port at bind and nothing dials this
    // server; the test wires everything over the shared instance
    // (Codeberg #221).
    let daemon_tcp: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let instance_name = format!("lblogd-b2-test-{}", std::process::id());
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

    // The blog node: a shared-instance client of the daemon (the production
    // topology), serving the fixture posts.
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());
    let files_dir = tempfile::tempdir().expect("files dir");
    let picture = fixture_picture();
    std::fs::write(files_dir.path().join("antenne.png"), &picture).expect("write picture");
    let data_dir = tempfile::tempdir().expect("data dir");
    let sources = Sources::new(posts_dir.path()).with_files(Some(FileArea::new(files_dir.path())));
    let (reloader, content) = Reloader::new(fixture_meta(), sources).expect("initial content load");
    let blog = BlogNode::start(
        BlogNodeConfig {
            instance_name: instance_name.clone(),
            data_dir: data_dir.path().to_path_buf(),
            display_name: "lblogd test blog".to_string(),
            announce_interval: Duration::from_secs(3600),
        },
        content,
    )
    .await
    .expect("start blog node");
    let dest_hex = hex::encode(blog.destination_hash().as_bytes());
    let blog_task = tokio::spawn(blog.run());
    // Let the node's IPC link establish and its announce propagate.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // What the node must serve: exactly the renderer output over the same
    // fixture directory.
    let posts = load_posts_dir(posts_dir.path()).expect("load fixture posts");
    let expected_index = render_index_micron(&fixture_meta(), &posts).into_bytes();
    let small = posts
        .iter()
        .find(|p| p.slug == "second")
        .expect("second fixture post");
    let expected_small = render_post_micron(&fixture_meta(), small).into_bytes();
    let large = posts
        .iter()
        .find(|p| p.slug == "large")
        .expect("large fixture post");
    let expected_large = render_post_micron(&fixture_meta(), large).into_bytes();
    assert!(
        expected_large.len() > 262_144,
        "large post must exceed the max link MDU to force the resource path (got {})",
        expected_large.len()
    );

    // The client: lnomad's real fetch path over its own IPC connection.
    let session_storage = tempfile::tempdir().expect("session storage");
    let app_dir = tempfile::tempdir().expect("session app dir");
    let mut session = Session::connect_to_with_app_dir(
        &instance_name,
        session_storage.path().to_path_buf(),
        Some(app_dir.path().to_path_buf()),
    )
    .await
    .expect("lnomad session connect");

    // Index and a small post: single-packet RESPONSE path, byte-exact.
    let target = parse_url(&format!("{dest_hex}:/page/index.mu"), None).expect("parse index url");
    let page = session
        .fetch(&target, Duration::from_secs(20))
        .await
        .expect("fetch index page");
    assert_eq!(
        page, expected_index,
        "fetched index must be byte-exactly the rendered index"
    );

    let target = parse_url(&format!("{dest_hex}:/page/second.mu"), None).expect("parse post url");
    let page = session
        .fetch(&target, Duration::from_secs(20))
        .await
        .expect("fetch small post");
    assert_eq!(
        page, expected_small,
        "fetched post must be byte-exactly the rendered post"
    );

    // The large post: send_response returns PayloadTooLarge, the node falls
    // back to send_response_resource, and the client reassembles the full
    // page from the is_response Resource.
    let target = parse_url(&format!("{dest_hex}:/page/large.mu"), None).expect("parse large url");
    let page = session
        .fetch(&target, Duration::from_secs(60))
        .await
        .expect("fetch large post over the resource path");
    assert_eq!(
        page, expected_large,
        "large post must round-trip byte-exactly over the resource path"
    );

    // A picture from the file area: NomadNet's `serve_file` wire form, which
    // is a Resource of the RAW bytes plus msgpack metadata naming the file —
    // no msgpack `bytes` wrapper, unlike a page. This is the whole reason an
    // image can reach a mesh reader at all, micron having no image construct,
    // so it is asserted through lnomad's real download path.
    let target = parse_url(&format!("{dest_hex}:/file/antenne.png"), None).expect("parse file url");
    let (bytes, name) = session
        .download_file(&target, Duration::from_secs(60))
        .await
        .expect("download the picture");
    assert_eq!(
        bytes, picture,
        "the picture must arrive byte for byte, with no page wrapper around it"
    );
    assert_eq!(
        name.as_deref(),
        Some("antenne.png"),
        "the response metadata must name the file, as NomadNet's serve_file does"
    );

    // An unknown file is a clean timeout, exactly like an unknown page.
    let target =
        parse_url(&format!("{dest_hex}:/file/nicht-da.png"), None).expect("parse file url");
    let result = session.download_file(&target, Duration::from_secs(2)).await;
    assert!(
        matches!(result, Err(FetchError::Timeout)),
        "an unknown file must surface a clean Timeout, got {result:?}"
    );

    // Unknown path: the stack drops it silently (no 404 in the protocol),
    // the client sees a clean timeout, nothing crashes.
    let target = parse_url(&format!("{dest_hex}:/page/nope.mu"), None).expect("parse bad url");
    let result = session.fetch(&target, Duration::from_secs(2)).await;
    assert!(
        matches!(result, Err(FetchError::Timeout)),
        "unknown path must surface a clean Timeout, got {result:?}"
    );

    // A reload registers handlers for new pages and deregisters vanished
    // ones, both visible over the same live link without restarting anything.
    std::fs::write(
        posts_dir.path().join("third.md"),
        "+++\ntitle = \"Third Post\"\ndate = \"2026-07-20\"\nslug = \"third\"\n+++\n\nAdded at runtime.\n",
    )
    .expect("write third.md");
    std::fs::remove_file(posts_dir.path().join("second.md")).expect("remove second.md");
    reloader.reload().expect("reload");
    // The node applies the swap on its select! loop; give it a turn.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let reloaded = load_posts_dir(posts_dir.path()).expect("load reloaded posts");
    let third = reloaded
        .iter()
        .find(|p| p.slug == "third")
        .expect("third fixture post");
    let target = parse_url(&format!("{dest_hex}:/page/third.mu"), None).expect("parse third url");
    let page = session
        .fetch(&target, Duration::from_secs(20))
        .await
        .expect("fetch post added by reload");
    assert_eq!(
        page,
        render_post_micron(&fixture_meta(), third).into_bytes(),
        "a post added by reload must be served byte-exactly"
    );

    let target = parse_url(&format!("{dest_hex}:/page/index.mu"), None).expect("parse index url");
    let page = session
        .fetch(&target, Duration::from_secs(20))
        .await
        .expect("fetch reloaded index");
    assert_eq!(
        page,
        render_index_micron(&fixture_meta(), &reloaded).into_bytes(),
        "the index must reflect the reloaded post set"
    );

    // A picture added at runtime is served after the reload, and one removed
    // stops being served: the file area reconciles like the pages do.
    std::fs::write(files_dir.path().join("mast.jpg"), b"jpeg-ish bytes").expect("write mast.jpg");
    std::fs::remove_file(files_dir.path().join("antenne.png")).expect("remove antenne.png");
    reloader.reload().expect("reload the file area");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let target = parse_url(&format!("{dest_hex}:/file/mast.jpg"), None).expect("parse file url");
    let (bytes, name) = session
        .download_file(&target, Duration::from_secs(20))
        .await
        .expect("download the picture added by reload");
    assert_eq!(bytes, b"jpeg-ish bytes");
    assert_eq!(name.as_deref(), Some("mast.jpg"));

    let target = parse_url(&format!("{dest_hex}:/file/antenne.png"), None).expect("parse file url");
    let result = session.download_file(&target, Duration::from_secs(2)).await;
    assert!(
        matches!(result, Err(FetchError::Timeout)),
        "a file removed by reload must stop being served, got {result:?}"
    );

    // A removed post stops being served: a clean timeout rather than stale
    // content. What this pins is the page lookup in `respond`, not the handler
    // deregistration — a leaked handler lets the request reach the node, which
    // then drops it for the missing page, and the client sees the same timeout
    // either way (verified by deleting the deregistration loop: this test still
    // passed). The deregistration decision is pinned by
    // `node::tests::a_vanished_page_is_deregistered_and_a_new_one_registered`.
    let target = parse_url(&format!("{dest_hex}:/page/second.mu"), None).expect("parse second url");
    let result = session.fetch(&target, Duration::from_secs(2)).await;
    assert!(
        matches!(result, Err(FetchError::Timeout)),
        "a post removed by reload must stop being served, got {result:?}"
    );

    session.close().await.expect("close session");
    blog_task.abort();
    daemon.stop().await.expect("stop daemon");
}

/// Fixture picture bytes: a PNG signature followed by a run of values that is
/// deliberately not valid UTF-8, so any decode-and-re-encode on the way would
/// show up as a mismatch rather than pass silently.
fn fixture_picture() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend((0..4096).map(|i| (i % 256) as u8));
    bytes
}

/// Blog metadata for this fixture: identity is not what the test is about.
fn fixture_meta() -> BlogMeta {
    BlogMeta {
        title: "lblogd test blog".to_string(),
        language: "en".to_string(),
        ..BlogMeta::default()
    }
}
