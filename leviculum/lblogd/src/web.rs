//! The clearnet web server: serves the blog as HTML over HTTP and HTTPS.
//!
//! HTTPS certificates come from Let's Encrypt automatically via `rustls-acme`
//! using the TLS-ALPN-01 challenge, which rides the HTTPS listener itself:
//! the ACME validation connection arrives on port 443 with a special ALPN
//! value and is answered inside the TLS acceptor, so no challenge plumbing
//! exists at the HTTP layer. Certificates and the account key are cached in a
//! persistent directory so restarts and renewals do not re-register.
//!
//! In that deployment mode the plain-HTTP listener does exactly one thing:
//! 301-redirect every request to the `https://` equivalent.
//!
//! Setting [`WebConfig::acme`] to `None` selects the plaintext development
//! mode instead: no HTTPS listener and no ACME traffic at all, and the HTTP
//! listener serves the blog directly. Certificate acquisition needs a
//! publicly reachable domain, so without this mode the server cannot be run
//! at all on a developer machine.
//!
//! Posts come from the shared [`SnapshotRx`] channel and are rendered per
//! request from whatever snapshot is current, so a reload is picked up by the
//! next request without restarting the listener.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;
use thiserror::Error;

use crate::content::SnapshotRx;
use crate::counter::Counter;
use crate::files;
use crate::render::{
    render_about_html, render_feed_atom, render_index_html, render_post_html, ABOUT_HTML_PATH,
    FEED_PATH,
};

/// Errors from starting or running the web server.
#[derive(Debug, Error)]
pub enum WebError {
    /// The config lists no domains to obtain a certificate for.
    #[error("no domains configured")]
    NoDomains,
    /// Binding a listen address failed.
    #[error("binding {addr}: {source}")]
    Bind {
        /// The address that failed to bind.
        addr: SocketAddr,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The HTTP redirect server failed.
    #[error("http server: {0}")]
    Http(std::io::Error),
    /// The HTTPS server failed.
    #[error("https server: {0}")]
    Https(std::io::Error),
    /// The ACME certificate state stream ended (it is designed to run
    /// forever, so this indicates a bug in the ACME layer).
    #[error("acme certificate state stream ended unexpectedly")]
    AcmeEnded,
}

/// Let's Encrypt settings, present exactly when HTTPS is wanted.
#[derive(Clone, Debug)]
pub struct AcmeSettings {
    /// Domains the certificate covers; the first one doubles as the redirect
    /// target when a request carries no Host header.
    pub domains: Vec<String>,
    /// Persistent directory caching the ACME account key and certificates.
    /// Losing it forces re-issuance on every start, which burns Let's
    /// Encrypt rate limits.
    pub cache_dir: PathBuf,
    /// Contact email for the ACME account (expiry warnings and the like).
    pub contact_email: String,
    /// Use the Let's Encrypt STAGING directory instead of production.
    /// Staging issues untrusted certificates but has generous rate limits;
    /// production is a deliberate config choice for real deployments.
    pub staging: bool,
}

/// Configuration for [`run_web`].
#[derive(Clone, Debug)]
pub struct WebConfig {
    /// `Some`: obtain HTTPS certificates from Let's Encrypt and make the
    /// plain-HTTP listener redirect-only. `None`: plaintext development
    /// mode, where [`http_bind`](Self::http_bind) serves the blog itself and
    /// no HTTPS listener is opened.
    pub acme: Option<AcmeSettings>,
    /// Plain-HTTP listen address (normally port 80). Redirect-only with ACME
    /// enabled, the blog itself without it.
    pub http_bind: SocketAddr,
    /// HTTPS listen address (normally port 443). TLS-ALPN-01 validation
    /// requires the ACME server to reach the certificate domains on port
    /// 443, so in real deployments this must be reachable there. Unused, and
    /// never bound, when [`acme`](Self::acme) is `None`.
    pub https_bind: SocketAddr,
}

/// Build the blog router: `/` is the post index, `/posts/{slug}` one post,
/// `/files/{name}` a file from the file area, everything else a small HTML
/// 404.
///
/// The handlers read the snapshot per request, so a reload takes effect on
/// the next request without touching the listener.
pub fn build_router(content: SnapshotRx) -> Router {
    build_router_counting(content, Arc::new(Counter::disabled()))
}

/// [`build_router`], with every request counted into `counter`.
pub fn build_router_counting(content: SnapshotRx, counter: Arc<Counter>) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/posts/{slug}", get(post_page))
        .route(ABOUT_HTML_PATH, get(about_page))
        .route(FEED_PATH, get(feed))
        .route(&format!("{}{{name}}", files::WEB_PREFIX), get(file_asset))
        .fallback(fallback_page)
        .with_state(content)
        .layer(middleware::from_fn_with_state(counter, count_request))
}

/// Count one request, by status only.
///
/// A layer rather than a line in each handler so the fallback is covered too:
/// a 404 is a request, and the number of them is exactly what tells a reader
/// how much of the total was somebody scanning for `/wp-login.php`.
///
/// What is deliberately absent is the peer address. It never reaches this
/// function — the router is served with [`Router::into_make_service`], not
/// the `with_connect_info` variant, so there is no `ConnectInfo` to extract
/// and no way for a later edit to start retaining one by accident. A count
/// does not need to know who; see [`crate::counter`] for the argument.
async fn count_request(
    State(counter): State<Arc<Counter>>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    counter.web_request(response.status() != StatusCode::NOT_FOUND);
    response
}

/// One file from the file area: the same bytes the mesh side serves under
/// `/file/<name>`, with a content type from the extension.
///
/// Served through the snapshot rather than straight off the filesystem, so
/// the two sides can never disagree about which files exist — a name the
/// mesh will not serve (too large, unusable name) is a 404 here too. That is
/// also why there is no `ServeDir`: it would answer from the directory, not
/// from the snapshot.
async fn file_asset(State(content): State<SnapshotRx>, UrlPath(name): UrlPath<String>) -> Response {
    let snapshot = content.borrow().clone();
    // axum has already percent-decoded the segment, so a `%2e%2e%2f` attempt
    // arrives here as `../` and dies on the separator check.
    let Some(entry) = files::sanitize_name(&name).and_then(|name| snapshot.files.get(&name)) else {
        return not_found();
    };
    let max_bytes = snapshot
        .file_area
        .as_ref()
        .map(|area| area.max_bytes)
        .unwrap_or(files::DEFAULT_MAX_FILE_BYTES);
    match files::read_entry(entry, max_bytes) {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, files::mime_for(&entry.name))],
            bytes,
        )
            .into_response(),
        Err(e) => {
            // The snapshot says it exists but the disk disagrees: it was
            // deleted or replaced since the load, and the next reload will
            // drop it. A 404 is the honest answer in the meantime.
            eprintln!("lblogd: cannot serve {}: {e}", entry.name);
            not_found()
        }
    }
}

/// The Atom feed, or a 404 when the blog has no public URL to build absolute
/// links from (a plaintext development run).
async fn feed(State(content): State<SnapshotRx>) -> Response {
    let snapshot = content.borrow().clone();
    match render_feed_atom(&snapshot.meta, &snapshot.posts) {
        Some(xml) => (
            [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
            xml,
        )
            .into_response(),
        None => not_found(),
    }
}

/// The about page, or a 404 when nothing is configured for it.
async fn about_page(State(content): State<SnapshotRx>) -> Response {
    let snapshot = content.borrow().clone();
    match snapshot.meta.has_about {
        true => Html(render_about_html(
            &snapshot.meta,
            &snapshot.css,
            snapshot.about.as_ref(),
        ))
        .into_response(),
        false => not_found(),
    }
}

async fn index_page(State(content): State<SnapshotRx>) -> Html<String> {
    // Clone the Arc out of the watch borrow immediately: the borrow holds a
    // read lock, and holding one across rendering would block reloads.
    let snapshot = content.borrow().clone();
    Html(render_index_html(
        &snapshot.meta,
        &snapshot.css,
        &snapshot.posts,
    ))
}

async fn post_page(State(content): State<SnapshotRx>, UrlPath(slug): UrlPath<String>) -> Response {
    let snapshot = content.borrow().clone();
    match snapshot.posts.iter().find(|p| p.slug == slug) {
        Some(post) => Html(render_post_html(&snapshot.meta, &snapshot.css, post)).into_response(),
        None => not_found(),
    }
}

async fn fallback_page() -> Response {
    not_found()
}

fn not_found() -> Response {
    const BODY: &str = "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">\
                        <title>404 Not Found</title></head>\n\
                        <body><h1>404 Not Found</h1></body>\n</html>\n";
    (StatusCode::NOT_FOUND, Html(BODY)).into_response()
}

/// Serve the blog and run until a server fails.
///
/// With [`WebConfig::acme`] set, that means HTTPS with automatic Let's
/// Encrypt certificates plus a plain-HTTP listener that 301-redirects to it;
/// without it, a single plain-HTTP listener serving the blog directly.
pub async fn run_web(
    config: WebConfig,
    content: SnapshotRx,
    counter: Arc<Counter>,
) -> Result<(), WebError> {
    let router = build_router_counting(content, counter);
    match config.acme {
        Some(acme) => serve_https(router, acme, config.http_bind, config.https_bind).await,
        None => serve_plain(router, config.http_bind).await,
    }
}

/// Plaintext development mode: one listener, no TLS, no ACME.
async fn serve_plain(router: Router, http_bind: SocketAddr) -> Result<(), WebError> {
    let listener = tokio::net::TcpListener::bind(http_bind)
        .await
        .map_err(|source| WebError::Bind {
            addr: http_bind,
            source,
        })?;
    axum::serve(listener, router.into_make_service())
        .await
        .map_err(WebError::Http)
}

/// [`run_web`]'s plaintext mode on a listener the caller already bound.
///
/// The caller binding `:0` and keeping the listener is the only way to serve
/// on a kernel-assigned port without a probe-then-rebind window (Codeberg
/// #221); the tests use this so no free-port probe races the bind.
pub async fn run_web_plain_on(
    listener: std::net::TcpListener,
    content: SnapshotRx,
    counter: Arc<Counter>,
) -> Result<(), WebError> {
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| ([127, 0, 0, 1], 0).into());
    let listener = listener
        .set_nonblocking(true)
        .and_then(|()| tokio::net::TcpListener::from_std(listener))
        .map_err(|source| WebError::Bind { addr, source })?;
    let router = build_router_counting(content, counter);
    axum::serve(listener, router.into_make_service())
        .await
        .map_err(WebError::Http)
}

/// Deployment mode: HTTPS with Let's Encrypt certificates, plain HTTP
/// redirecting to it.
///
/// The certificate acquisition path (rustls-acme against Let's Encrypt) is
/// compile-verified only: it needs a publicly reachable domain, so it is
/// exercised in real deployment, not in CI.
async fn serve_https(
    router: Router,
    acme: AcmeSettings,
    http_bind: SocketAddr,
    https_bind: SocketAddr,
) -> Result<(), WebError> {
    if acme.domains.is_empty() {
        return Err(WebError::NoDomains);
    }

    let mut acme_state = AcmeConfig::new(&acme.domains)
        .contact_push(format!("mailto:{}", acme.contact_email))
        .cache(DirCache::new(acme.cache_dir.clone()))
        .directory_lets_encrypt(!acme.staging)
        .state();
    let acceptor = acme_state.axum_acceptor(acme_state.default_rustls_config());
    // The state stream drives ACME ordering and renewal; it must be polled
    // for the acceptor to ever have a certificate. It never terminates.
    let acme_driver = async move {
        while let Some(event) = acme_state.next().await {
            match event {
                Ok(ok) => eprintln!("lblogd: acme: {ok:?}"),
                Err(err) => eprintln!("lblogd: acme error: {err:?}"),
            }
        }
    };

    let https_server = axum_server::bind(https_bind)
        .acceptor(acceptor)
        .serve(router.into_make_service());

    let redirect = redirect_router(https_bind.port(), acme.domains[0].clone());
    let http_listener = tokio::net::TcpListener::bind(http_bind)
        .await
        .map_err(|source| WebError::Bind {
            addr: http_bind,
            source,
        })?;
    let http_server = axum::serve(http_listener, redirect.into_make_service());

    tokio::select! {
        result = https_server => result.map_err(WebError::Https),
        result = http_server.into_future() => result.map_err(WebError::Http),
        () = acme_driver => Err(WebError::AcmeEnded),
    }
}

/// The redirect-only router served on the plain-HTTP listener: every request
/// gets a 301 to its `https://` equivalent.
fn redirect_router(https_port: u16, fallback_host: String) -> Router {
    Router::new().fallback(move |headers: HeaderMap, uri: Uri| {
        let target = redirect_target(&headers, &uri, https_port, &fallback_host);
        async move { (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, target)]) }
    })
}

/// The `https://` URL a plain-HTTP request redirects to: the request's Host
/// (sans port, falling back to the first configured domain), the HTTPS port
/// unless it is the default 443, and the original path and query.
fn redirect_target(headers: &HeaderMap, uri: &Uri, https_port: u16, fallback_host: &str) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_without_port)
        .filter(|h| !h.is_empty())
        .unwrap_or(fallback_host);
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    if https_port == 443 {
        format!("https://{host}{path}")
    } else {
        format!("https://{host}:{https_port}{path}")
    }
}

/// Strip a `:port` suffix from a Host header value, keeping IPv6 literals
/// (`[::1]:80` becomes `[::1]`) intact.
fn host_without_port(host: &str) -> &str {
    if let Some(end) = host.rfind(']') {
        return &host[..=end];
    }
    match host.rfind(':') {
        Some(idx) => &host[..idx],
        None => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tokio::sync::watch;
    use tower::ServiceExt;

    use crate::content::Snapshot;
    use crate::post::Post;
    use crate::render::BlogMeta;

    fn test_meta() -> BlogMeta {
        BlogMeta {
            title: "Test Blog".to_string(),
            language: "en".to_string(),
            ..BlogMeta::default()
        }
    }

    fn sample_posts() -> Vec<Post> {
        vec![
            Post {
                title: "Hello World".to_string(),
                date: "2026-07-02".parse().unwrap(),
                author: None,
                slug: "hello-world".to_string(),
                body_md: "First **post** body.".to_string(),
            },
            Post {
                title: "Older Post".to_string(),
                date: "2026-06-30".parse().unwrap(),
                author: None,
                slug: "older-post".to_string(),
                body_md: "Nothing to see.".to_string(),
            },
        ]
    }

    /// A router over a fixed snapshot, plus the sender that can replace it.
    fn router_over(posts: Vec<Post>) -> (Router, watch::Sender<Arc<Snapshot>>) {
        let snapshot = Arc::new(Snapshot {
            meta: test_meta(),
            posts,
            ..Snapshot::default()
        });
        let (tx, rx) = watch::channel(snapshot);
        (build_router(rx), tx)
    }

    fn sample_router() -> Router {
        router_over(sample_posts()).0
    }

    async fn get(router: Router, path: &str) -> (StatusCode, HeaderMap, String) {
        let response = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn index_lists_posts_with_links() {
        let (status, headers, body) = get(sample_router(), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(body.contains("Hello World"));
        assert!(body.contains("/posts/hello-world"));
        assert!(body.contains("/posts/older-post"));
    }

    #[tokio::test]
    async fn post_page_renders_body() {
        let (status, headers, body) = get(sample_router(), "/posts/hello-world").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(body.contains("Hello World"));
        assert!(body.contains("<strong>post</strong>"));
    }

    #[tokio::test]
    async fn unknown_slug_is_404() {
        let (status, _, body) = get(sample_router(), "/posts/does-not-exist").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("404"));
    }

    #[tokio::test]
    async fn a_reload_is_visible_to_the_next_request() {
        // The router is built once and never rebuilt, so this is the property
        // that makes reloading possible without restarting the listener.
        let (router, tx) = router_over(sample_posts());

        let (status, _, _) = get(router.clone(), "/posts/hello-world").await;
        assert_eq!(status, StatusCode::OK);

        let mut posts = sample_posts();
        posts.retain(|p| p.slug != "hello-world");
        posts.push(Post {
            title: "Fresh Post".to_string(),
            date: "2026-07-27".parse().unwrap(),
            author: None,
            slug: "fresh-post".to_string(),
            body_md: "Brand new.".to_string(),
        });
        tx.send(Arc::new(Snapshot {
            meta: test_meta(),
            posts,
            ..Snapshot::default()
        }))
        .unwrap();

        let (status, _, body) = get(router.clone(), "/posts/fresh-post").await;
        assert_eq!(status, StatusCode::OK, "the new post must be served");
        assert!(body.contains("Fresh Post"), "{body}");

        let (status, _, _) = get(router, "/posts/hello-world").await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a removed post must stop being served"
        );
    }

    #[tokio::test]
    async fn every_request_is_counted_and_the_404s_are_separable() {
        // A bot scanning for /wp-login.php is a request and is counted as
        // one; what keeps it from quietly inflating a number about reading is
        // that it is also counted as a miss.
        let dir = tempfile::tempdir().unwrap();
        let counter = Arc::new(Counter::open(dir.path().join("counts.log")).unwrap());
        let snapshot = Arc::new(Snapshot {
            meta: test_meta(),
            posts: sample_posts(),
            ..Snapshot::default()
        });
        let (_tx, rx) = watch::channel(snapshot);
        let router = build_router_counting(rx, Arc::clone(&counter));

        for path in ["/", "/posts/hello-world", "/wp-login.php"] {
            get(router.clone(), path).await;
        }

        let (_, counts) = counter.open_day();
        assert_eq!(counts.web_requests, 3, "the fallback route counts too");
        assert_eq!(counts.web_not_found, 1);
        assert_eq!(
            counts.mesh_requests, 0,
            "the web layer must not touch the mesh side's numbers"
        );
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let (status, _, body) = get(sample_router(), "/random").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("404"));
    }

    #[tokio::test]
    async fn redirect_router_301s_to_https() {
        let response = redirect_router(443, "blog.example".to_string())
            .oneshot(
                Request::builder()
                    .uri("/posts/hello-world?x=1")
                    .header(header::HOST, "blog.example:80")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://blog.example/posts/hello-world?x=1"
        );
    }

    #[test]
    fn redirect_target_shapes() {
        let uri: Uri = "/a/b?q=1".parse().unwrap();
        let mut headers = HeaderMap::new();

        // No Host header: fall back to the configured domain.
        assert_eq!(
            redirect_target(&headers, &uri, 443, "blog.example"),
            "https://blog.example/a/b?q=1"
        );

        // Host with port: port stripped, default HTTPS port omitted.
        headers.insert(header::HOST, "other.example:8080".parse().unwrap());
        assert_eq!(
            redirect_target(&headers, &uri, 443, "blog.example"),
            "https://other.example/a/b?q=1"
        );

        // Non-default HTTPS port is appended.
        assert_eq!(
            redirect_target(&headers, &uri, 8443, "blog.example"),
            "https://other.example:8443/a/b?q=1"
        );

        // IPv6 literal keeps its brackets, loses its port.
        headers.insert(header::HOST, "[::1]:8080".parse().unwrap());
        assert_eq!(
            redirect_target(&headers, &uri, 443, "blog.example"),
            "https://[::1]/a/b?q=1"
        );
    }

    #[test]
    fn host_without_port_shapes() {
        assert_eq!(host_without_port("example.com"), "example.com");
        assert_eq!(host_without_port("example.com:80"), "example.com");
        assert_eq!(host_without_port("[::1]"), "[::1]");
        assert_eq!(host_without_port("[::1]:8080"), "[::1]");
    }
}
