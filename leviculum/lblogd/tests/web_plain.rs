//! The plaintext development mode of the web server: with ACME disabled the
//! HTTP listener serves the blog itself instead of redirecting to HTTPS.
//!
//! The requests go over a real TCP socket rather than through `tower`'s
//! oneshot service call, so this covers what the router unit tests cannot:
//! that `run_web` actually binds, accepts, and answers on `http_bind` without
//! touching Let's Encrypt. The ACME path stays compile-verified only (it
//! needs a publicly reachable domain), but the branch guarding it is asserted
//! here too.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use lblogd::content::{Reloader, SnapshotRx, Sources};
use lblogd::counter::Counter;
use lblogd::files::FileArea;
use lblogd::render::BlogMeta;
use lblogd::web::{run_web, run_web_plain_on, AcmeSettings, WebConfig, WebError};

fn write_fixture_posts(dir: &Path) {
    std::fs::write(
        dir.join("hello.md"),
        "+++\ntitle = \"Hello Mesh\"\ndate = \"2026-07-01\"\n+++\n\nFirst post, **small** enough for one packet.\n",
    )
    .expect("write hello.md");
}

/// One plain HTTP/1.1 GET, returning the response's raw bytes. Needed where
/// the body is binary: [`http_get`] decodes lossily, which would replace every
/// non-UTF-8 byte of a picture with U+FFFD and hide exactly the corruption
/// worth testing for.
async fn http_get_bytes(addr: SocketAddr, path: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header/body separator");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    (head, raw[split + 4..].to_vec())
}

/// One plain HTTP/1.1 GET, returning the whole raw response. `Connection:
/// close` makes the server end the stream, so reading to EOF terminates.
async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    String::from_utf8_lossy(&raw).into_owned()
}

/// Start a plaintext server over `content` on a kernel-assigned port.
///
/// The test binds `:0` itself, keeps the listener, and hands it over
/// (Codeberg #221): the port is held from the instant it exists, so no
/// free-port probe races the bind, and connections queue in the backlog even
/// before the server task first accepts — no listener-poll needed.
async fn serve_plain(content: SnapshotRx) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let bind = listener.local_addr().expect("local addr");
    tokio::spawn(run_web_plain_on(
        listener,
        content,
        // These tests are about serving, not counting; a disabled counter
        // keeps them off the filesystem.
        std::sync::Arc::new(Counter::disabled()),
    ));
    bind
}

/// `run_web`'s own plaintext bind path, pinned via its error branch: an
/// occupied address is a `WebError::Bind`, deterministically — the test holds
/// the occupant itself.
#[tokio::test]
async fn run_web_reports_a_taken_address_as_bind_error() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());
    let (_reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");

    let occupant = TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let taken = occupant.local_addr().expect("local addr");
    let err = run_web(
        WebConfig {
            acme: None,
            http_bind: taken,
            // Unused in plaintext mode; a bogus value must not be bound.
            https_bind: "127.0.0.1:1".parse().expect("parse https bind"),
        },
        content,
        std::sync::Arc::new(Counter::disabled()),
    )
    .await
    .expect_err("binding an occupied address must fail");
    assert!(matches!(err, WebError::Bind { .. }), "{err}");
}

#[tokio::test]
async fn plain_http_serves_the_blog_without_acme() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());
    let (_reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;

    let index = http_get(bind, "/").await;
    assert!(
        index.starts_with("HTTP/1.1 200"),
        "index must be served, not redirected: {index}"
    );
    assert!(index.contains("Hello Mesh"), "{index}");
    assert!(index.contains("/posts/hello-mesh"), "{index}");

    let post = http_get(bind, "/posts/hello-mesh").await;
    assert!(post.starts_with("HTTP/1.1 200"), "{post}");
    assert!(post.contains("<strong>small</strong>"), "{post}");

    let missing = http_get(bind, "/nicht-da").await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
}

#[tokio::test]
async fn the_file_area_is_served_verbatim_and_nothing_else_is() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    let files_dir = tempfile::tempdir().expect("files dir");
    write_fixture_posts(posts_dir.path());
    // Deliberately not valid UTF-8: a picture is bytes, and the server must
    // hand them back unchanged.
    let pixels: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xc3,
    ];
    std::fs::write(files_dir.path().join("antenne.png"), &pixels).expect("write picture");
    std::fs::write(files_dir.path().join("huge.png"), vec![0u8; 4096]).expect("write huge");

    let sources = Sources::new(posts_dir.path()).with_files(Some(FileArea {
        dir: files_dir.path().to_path_buf(),
        max_bytes: 1024,
    }));
    let (_reloader, content) = Reloader::new(fixture_meta(), sources).expect("initial load");
    let bind = serve_plain(content).await;

    let (head, body) = http_get_bytes(bind, "/files/antenne.png").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("content-type: image/png"), "{head}");
    assert_eq!(body, pixels, "the picture must come back byte for byte");

    // A file the mesh side refuses to serve is not served here either: one
    // snapshot, one answer to "does this exist".
    let (head, _) = http_get_bytes(bind, "/files/huge.png").await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");

    let (head, _) = http_get_bytes(bind, "/files/nicht-da.png").await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
}

#[tokio::test]
async fn a_file_request_cannot_climb_out_of_the_area() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    let files_dir = tempfile::tempdir().expect("files dir");
    write_fixture_posts(posts_dir.path());
    std::fs::write(files_dir.path().join("antenne.png"), b"pixels").expect("write picture");
    let secret = files_dir.path().parent().unwrap().join("secret.txt");
    std::fs::write(&secret, b"not for readers").expect("write secret");

    let sources = Sources::new(posts_dir.path()).with_files(Some(FileArea::new(files_dir.path())));
    let (_reloader, content) = Reloader::new(fixture_meta(), sources).expect("initial load");
    let bind = serve_plain(content).await;

    // Percent-encoded, because axum decodes the segment before the handler
    // sees it: this is what actually reaches the name check.
    for path in [
        "/files/..%2Fsecret.txt",
        "/files/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/files/%2e%2e",
        "/files/.hidden",
    ] {
        let (head, body) = http_get_bytes(bind, path).await;
        assert!(
            head.starts_with("HTTP/1.1 404") || head.starts_with("HTTP/1.1 30"),
            "{path} must not be served: {head}"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains("not for readers"),
            "{path} escaped the file area"
        );
    }
}

#[tokio::test]
async fn a_reload_reaches_the_running_listener() {
    // The listener is bound once and never rebound, so this proves publishing
    // needs no restart: same socket, new content.
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());
    let (reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;

    assert!(
        http_get(bind, "/posts/zweiter-post")
            .await
            .starts_with("HTTP/1.1 404"),
        "the post does not exist yet"
    );

    std::fs::write(
        posts_dir.path().join("zweiter.md"),
        "+++\ntitle = \"Zweiter Post\"\ndate = \"2026-07-27\"\n+++\n\nFrisch dazugekommen.\n",
    )
    .expect("write second post");
    reloader.reload().expect("reload");

    let page = http_get(bind, "/posts/zweiter-post").await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("Zweiter Post"), "{page}");

    let index = http_get(bind, "/").await;
    assert!(index.contains("/posts/zweiter-post"), "{index}");

    // And removing it again takes it back out, on the same listener.
    std::fs::remove_file(posts_dir.path().join("zweiter.md")).expect("remove second post");
    reloader.reload().expect("reload after removal");
    assert!(
        http_get(bind, "/posts/zweiter-post")
            .await
            .starts_with("HTTP/1.1 404"),
        "a removed post must stop being served"
    );
}

#[tokio::test]
async fn a_failed_reload_keeps_the_server_serving() {
    // A typo in a post must not take a running blog offline.
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());
    let (reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;

    std::fs::write(
        posts_dir.path().join("kaputt.md"),
        "+++\ntitle = \"Kaputt\"\ndate = \"2026-13-45\"\n+++\n\nText.\n",
    )
    .expect("write broken post");
    let err = reloader
        .reload()
        .expect_err("a bad date must fail the load");
    assert!(err.to_string().contains("kaputt.md"), "{err}");

    let page = http_get(bind, "/posts/hello-mesh").await;
    assert!(
        page.starts_with("HTTP/1.1 200"),
        "the previous content must still be served: {page}"
    );
}

#[tokio::test]
async fn the_feed_route_answers_or_404s_by_configuration() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());

    // A plaintext development run has no public URL, so no feed.
    let (_reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;
    assert!(
        http_get(bind, "/feed.xml")
            .await
            .starts_with("HTTP/1.1 404"),
        "without a web url the route must 404 rather than serve broken links"
    );

    // With one, the same route serves the feed.
    let meta = BlogMeta {
        web_url: Some("https://example.test".to_string()),
        ..fixture_meta()
    };
    let (_reloader, content) =
        Reloader::new(meta, Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;
    let response = http_get(bind, "/feed.xml").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.contains("content-type: application/atom+xml; charset=utf-8"),
        "readers dispatch on the content type: {response}"
    );
    assert!(response.contains("<feed xmlns="), "{response}");
    assert!(
        response.contains("https://example.test/posts/hello-mesh"),
        "{response}"
    );
}

#[tokio::test]
async fn the_about_route_answers_or_404s_by_configuration() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());

    // Nothing configured for it: the route must not offer an empty page.
    let (_reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;
    assert!(
        http_get(bind, "/about").await.starts_with("HTTP/1.1 404"),
        "no about page configured, so no about route"
    );

    let meta = BlogMeta {
        author: Some("Lew Palm".to_string()),
        email: Some("lp@lew-palm.de".to_string()),
        has_about: true,
        ..fixture_meta()
    };
    let (_reloader, content) =
        Reloader::new(meta, Sources::new(posts_dir.path())).expect("initial load");
    let bind = serve_plain(content).await;

    let about = http_get(bind, "/about").await;
    assert!(about.starts_with("HTTP/1.1 200"), "{about}");
    assert!(about.contains("mailto:lp@lew-palm.de"), "{about}");

    // And the name on the index is now a link to it.
    let index = http_get(bind, "/").await;
    assert!(index.contains("<a href=\"/about\">Lew Palm</a>"), "{index}");
}

#[tokio::test]
async fn acme_mode_still_requires_domains() {
    let posts_dir = tempfile::tempdir().expect("posts dir");
    write_fixture_posts(posts_dir.path());
    let cache_dir = tempfile::tempdir().expect("acme cache dir");
    let (_reloader, content) =
        Reloader::new(fixture_meta(), Sources::new(posts_dir.path())).expect("initial load");

    let err = run_web(
        WebConfig {
            acme: Some(AcmeSettings {
                domains: Vec::new(),
                cache_dir: cache_dir.path().to_path_buf(),
                contact_email: "ops@example.org".to_string(),
                staging: true,
            }),
            http_bind: "127.0.0.1:1".parse().expect("parse http bind"),
            https_bind: "127.0.0.1:1".parse().expect("parse https bind"),
        },
        content,
        std::sync::Arc::new(Counter::disabled()),
    )
    .await
    .expect_err("empty domains must be rejected");
    assert!(matches!(err, WebError::NoDomains), "{err}");
}

/// Blog metadata for these fixtures.
fn fixture_meta() -> BlogMeta {
    BlogMeta {
        title: "Test Blog".to_string(),
        language: "en".to_string(),
        ..BlogMeta::default()
    }
}
