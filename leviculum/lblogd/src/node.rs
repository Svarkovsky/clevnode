//! The NomadNet page node: serves the blog's Micron pages over Reticulum.
//!
//! The node is a shared-instance client of a separately running `lnsd`
//! daemon (the same topology `lncp --listen` uses): it connects over IPC,
//! registers a `nomadnetwork.node` destination with one request handler per
//! page path, announces the destination under the blog's display name, and
//! answers page requests with the rendered Micron bytes. Small pages go out
//! as a single RESPONSE packet; pages larger than the link MDU fall back to
//! a response Resource.
//!
//! The file area is served from the same destination under `/file/<name>`,
//! the path NomadNet's `serve_file` uses, and answered with the same wire
//! form: a Resource carrying the raw bytes plus msgpack metadata naming the
//! file. That is how a picture reaches a mesh reader, micron having no image
//! construct of its own (see [`crate::files`]).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use leviculum_core::resource::ResourceError;
use leviculum_core::{LinkId, RequestError, RequestPolicy};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::{
    Destination, DestinationHash, DestinationType, Direction, Error as StdError, EventReceiver,
    Identity, NodeEvent, ProofStrategy, ReticulumNode,
};

use crate::content::{Snapshot, SnapshotRx};
use crate::counter::Counter;
use crate::files::{self, FileEntry};

/// Truncated request id length (matches the driver's request/response API).
const REQUEST_ID_LEN: usize = 16;

/// A queued large response: the request id it answers, the bytes, and which
/// wire form they go out in.
type PendingResponse = ([u8; REQUEST_ID_LEN], Vec<u8>, ResourceForm);

/// How a queued large response is sent once the link's resource slot frees.
///
/// The two forms are not interchangeable: a page is a response Resource whose
/// payload is the `[request_id, response]` wrapper, a file is a Resource of
/// raw bytes plus metadata. Sending a file the page way would hand the reader
/// a msgpack-wrapped picture.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ResourceForm {
    /// A rendered page.
    Page,
    /// A file from the file area, with the name its metadata carries.
    File(String),
}

/// Errors from building or running the blog node.
#[derive(Debug, Error)]
pub enum NodeError {
    /// Reading or writing the persistent identity file failed.
    #[error("identity file {path}: {source}")]
    IdentityIo {
        /// The identity file path.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The identity file holds no valid private key, or key export failed.
    #[error("identity {path}: {message}")]
    Identity {
        /// The identity file path.
        path: String,
        /// What went wrong.
        message: String,
    },
    /// Building the destination failed.
    #[error("destination: {0}")]
    Destination(String),
    /// A node operation (connect, start, announce) failed.
    #[error("node: {0}")]
    Node(#[from] StdError),
    /// The node was built without an event receiver.
    #[error("node has no event receiver")]
    NoEventReceiver,
}

/// Configuration for [`BlogNode::start`].
#[derive(Clone, Debug)]
pub struct BlogNodeConfig {
    /// Shared-instance name of the running `lnsd` daemon to serve through.
    pub instance_name: String,
    /// Data directory: the persistent identity lives at
    /// `<data_dir>/identities/lblogd`, node storage at `<data_dir>/storage`.
    pub data_dir: PathBuf,
    /// Display name announced as plain UTF-8 `app_data`, which is what shows
    /// up in NomadNet's Announce Stream and `lnomad --nodes`.
    pub display_name: String,
    /// Re-announce cadence. NomadNet nodes announce on the order of hours;
    /// [`BlogNodeConfig::default_announce_interval`] matches that.
    pub announce_interval: Duration,
}

impl BlogNodeConfig {
    /// The NomadNet-like default re-announce cadence.
    pub fn default_announce_interval() -> Duration {
        Duration::from_secs(6 * 60 * 60)
    }
}

/// A running blog page node: connected to the daemon, destination and page
/// handlers registered. Call [`run`](Self::run) to announce and serve.
pub struct BlogNode {
    node: ReticulumNode,
    events: EventReceiver,
    dest_hash: DestinationHash,
    display_name: String,
    announce_interval: Duration,
    /// The content currently served, replaced wholesale on reload.
    snapshot: Arc<Snapshot>,
    /// Reload channel; every change swaps [`snapshot`](Self::snapshot) and
    /// reconciles the registered request handlers with it.
    content: SnapshotRx,
    /// Large responses waiting for the link's outgoing resource slot: a link
    /// carries only one outgoing resource at a time, so concurrent large-page
    /// requests on the same link queue here until the slot frees up.
    pending: HashMap<LinkId, VecDeque<PendingResponse>>,
    /// Where requests and links are counted, disabled unless a caller hands
    /// over a real one via [`with_counter`](Self::with_counter).
    counter: Arc<Counter>,
}

impl BlogNode {
    /// Connect to the shared instance, load the identity, and register the
    /// destination and one request handler per page in the current snapshot.
    pub async fn start(config: BlogNodeConfig, content: SnapshotRx) -> Result<Self, NodeError> {
        let mut node = ReticulumNodeBuilder::new()
            .enable_transport(false)
            .connect_to_shared_instance(&config.instance_name)
            // Safe to share storage with lnsd: a client with
            // enable_transport(false) writes no paths, announces, or packet
            // hashes to storage. Identity is loaded separately.
            .storage_path(config.data_dir.join("storage"))
            .build_sync()?;
        let events = node
            .take_event_receiver()
            .ok_or(NodeError::NoEventReceiver)?;
        node.start().await?;

        // A blog whose destination hash changes on restart is useless, so the
        // identity persists across runs.
        let identity = load_or_generate_identity(&identity_path(&config.data_dir))?;
        let dest = blog_destination(identity)?;
        let dest_hash = *dest.hash();
        node.register_destination(dest);

        let snapshot = content.borrow().clone();
        // One handler per exact path: the wire carries only the truncated
        // path hash, so prefix or wildcard registration is impossible by
        // design. Unregistered paths are silently dropped by the stack.
        for path in snapshot
            .pages
            .keys()
            .cloned()
            .chain(snapshot.served_file_paths())
        {
            node.register_request_handler(dest_hash, &path, RequestPolicy::AllowAll);
        }

        Ok(BlogNode {
            node,
            events,
            dest_hash,
            display_name: config.display_name,
            announce_interval: config.announce_interval,
            snapshot,
            content,
            pending: HashMap::new(),
            counter: Arc::new(Counter::disabled()),
        })
    }

    /// Count this node's requests and links into `counter`.
    ///
    /// Not a [`BlogNodeConfig`] field: that struct is the config file's shape,
    /// and the counter is a live object shared with the web server.
    pub fn with_counter(mut self, counter: Arc<Counter>) -> Self {
        self.counter = counter;
        self
    }

    /// The node's destination hash (what a browser dials).
    pub fn destination_hash(&self) -> DestinationHash {
        self.dest_hash
    }

    /// The page request paths this node serves, sorted.
    pub fn served_paths(&self) -> Vec<String> {
        self.snapshot.served_paths()
    }

    /// The file request paths this node serves, sorted.
    pub fn served_file_paths(&self) -> Vec<String> {
        self.snapshot.served_file_paths()
    }

    /// Announce the destination and serve page requests until the daemon
    /// connection closes.
    pub async fn run(mut self) -> Result<(), NodeError> {
        self.node
            .announce_destination(&self.dest_hash, Some(self.display_name.as_bytes()))
            .await?;
        let mut announce = tokio::time::interval(self.announce_interval);
        announce.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                _ = announce.tick() => {
                    if let Err(e) = self
                        .node
                        .announce_destination(&self.dest_hash, Some(self.display_name.as_bytes()))
                        .await
                    {
                        eprintln!("lblogd: re-announce failed: {e}");
                    }
                }
                changed = self.content.changed() => {
                    // Err means the Reloader was dropped: no further reloads,
                    // but the current snapshot stays perfectly serveable.
                    if changed.is_ok() {
                        let snapshot = self.content.borrow_and_update().clone();
                        self.apply_snapshot(snapshot);
                    }
                }
                event = self.events.recv() => {
                    let Some(event) = event else {
                        return Ok(());
                    };
                    self.handle_event(event).await;
                }
            }
        }
    }

    /// Swap in new content and reconcile the registered handlers with it.
    ///
    /// Paths that vanished are deregistered, so a deleted post stops being
    /// served rather than lingering until restart; new paths are registered.
    /// Registration is an idempotent map insert, so unchanged paths are left
    /// alone rather than churned.
    ///
    /// In-flight transfers are unaffected: `respond` copies the page bytes out
    /// of the snapshot before sending, and queued responses in `pending` hold
    /// their own copies, so replacing the snapshot cannot tear a transfer that
    /// is already under way.
    fn apply_snapshot(&mut self, snapshot: Arc<Snapshot>) {
        let (gone, added) = reconcile_paths(&self.snapshot, &snapshot);
        for path in &gone {
            self.node.deregister_request_handler(self.dest_hash, path);
        }
        for path in &added {
            self.node
                .register_request_handler(self.dest_hash, path, RequestPolicy::AllowAll);
        }
        eprintln!(
            "lblogd: reloaded {} posts, serving {} pages and {} files",
            snapshot.posts.len(),
            snapshot.pages.len(),
            snapshot.files.len()
        );
        self.snapshot = snapshot;
    }

    async fn handle_event(&mut self, event: NodeEvent) {
        match event {
            NodeEvent::RequestReceived {
                link_id,
                request_id,
                path,
                ..
            } => {
                // The event carries no identity — a request is a path on a
                // link and nothing more. The link may hold one, if the peer
                // sent a LINKIDENTIFY, so ask it; for a public page nothing
                // asks a reader to, and the answer is expected to be None.
                self.counter
                    .mesh_request(self.node.get_remote_identity(&link_id).is_some());
                let page = self.snapshot.pages.get(&path).cloned();
                let file = self.snapshot.file_for_node_path(&path).cloned();
                match (page, file) {
                    (Some(bytes), _) => self.respond(link_id, request_id, bytes).await,
                    (None, Some(entry)) => self.respond_file(link_id, request_id, &entry).await,
                    // Unknown path: protocol-correct silent drop (there is no
                    // 404 in the protocol; the client sees a clean timeout).
                    (None, None) => {}
                }
            }
            // The outgoing resource slot on this link freed up (a large-page
            // transfer finished or died): send the next queued response.
            NodeEvent::ResourceCompleted {
                link_id,
                is_sender: true,
                ..
            }
            | NodeEvent::ResourceFailed {
                link_id,
                is_sender: true,
                ..
            } => {
                self.drain_pending(&link_id).await;
            }
            NodeEvent::LinkClosed { link_id, .. } => {
                self.pending.remove(&link_id);
            }
            // A reader arriving. This is the only honest session boundary the
            // mesh offers, and counting it here rather than deduplicating
            // request events by `link_id` is what keeps the counter's memory
            // flat: one increment, no set of live links to grow all day.
            NodeEvent::LinkEstablished {
                is_initiator: false,
                destination_hash,
                ..
            } if destination_hash == self.dest_hash => self.counter.mesh_session(),
            _ => {}
        }
    }

    /// Answer one request: single RESPONSE packet if it fits the link MDU,
    /// else a response Resource, queued per link if one is already in flight.
    async fn respond(&mut self, link_id: LinkId, request_id: [u8; REQUEST_ID_LEN], bytes: Vec<u8>) {
        match self.node.send_response(&link_id, &request_id, &bytes).await {
            Ok(()) => {}
            Err(StdError::Request(RequestError::PayloadTooLarge)) => {
                self.send_large(link_id, request_id, bytes, ResourceForm::Page)
                    .await;
            }
            Err(e) => eprintln!("lblogd: response failed: {e}"),
        }
    }

    /// Answer one file request the way NomadNet's `serve_file` does: a
    /// Resource carrying the raw bytes, plus msgpack metadata naming the file
    /// so the browser can save it under the name the author gave it.
    ///
    /// Always a Resource, never a single packet: a file is not a page, and a
    /// reader that asked for `antenne.jpg` must not receive a msgpack-wrapped
    /// value it would have to unwrap. NomadNet sends every file as a Resource
    /// regardless of size, and `lnomad`'s download path reads the response
    /// verbatim.
    ///
    /// A file that cannot be read is dropped silently, like an unknown path:
    /// the protocol has no error response, and the reader sees a timeout.
    async fn respond_file(
        &mut self,
        link_id: LinkId,
        request_id: [u8; REQUEST_ID_LEN],
        entry: &FileEntry,
    ) {
        let max_bytes = self
            .snapshot
            .file_area
            .as_ref()
            .map(|area| area.max_bytes)
            .unwrap_or(files::DEFAULT_MAX_FILE_BYTES);
        let bytes = match files::read_entry(entry, max_bytes) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("lblogd: cannot serve {}: {e}", entry.name);
                return;
            }
        };
        self.send_large(
            link_id,
            request_id,
            bytes,
            ResourceForm::File(entry.name.clone()),
        )
        .await;
    }

    async fn send_large(
        &mut self,
        link_id: LinkId,
        request_id: [u8; REQUEST_ID_LEN],
        bytes: Vec<u8>,
        form: ResourceForm,
    ) {
        let sent = match &form {
            ResourceForm::Page => {
                self.node
                    .send_response_resource(&link_id, &request_id, &bytes)
                    .await
            }
            ResourceForm::File(name) => {
                self.node
                    .send_file_response(&link_id, &request_id, &bytes, &file_metadata(name))
                    .await
            }
        };
        match sent {
            Ok(()) => {}
            // A link serves one outgoing resource at a time; queue until the
            // in-flight transfer completes or fails.
            Err(StdError::Resource(ResourceError::TransferInProgress)) => {
                self.pending
                    .entry(link_id)
                    .or_default()
                    .push_back((request_id, bytes, form));
            }
            Err(e) => eprintln!("lblogd: resource response failed: {e}"),
        }
    }

    async fn drain_pending(&mut self, link_id: &LinkId) {
        let Some(queue) = self.pending.get_mut(link_id) else {
            return;
        };
        let Some((request_id, bytes, form)) = queue.pop_front() else {
            self.pending.remove(link_id);
            return;
        };
        if queue.is_empty() {
            self.pending.remove(link_id);
        }
        // send_large re-queues on TransferInProgress (e.g. a multi-segment
        // transfer that only completed one segment), so nothing is lost.
        self.send_large(*link_id, request_id, bytes, form).await;
    }
}

/// The msgpack `{"name": "<name>"}` map a file response's Resource carries,
/// which is what NomadNet's `serve_file` sends and what `lnomad` reads to
/// name the saved file.
///
/// Encoding a two-element map cannot fail, so an error here would mean the
/// encoder itself broke; an empty metadata blob is the honest fallback, and
/// the reader then falls back to the URL basename.
fn file_metadata(name: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    let value = rmpv::Value::Map(vec![(
        rmpv::Value::String("name".into()),
        rmpv::Value::String(name.into()),
    )]);
    if rmpv::encode::write_value(&mut buf, &value).is_err() {
        buf.clear();
    }
    buf
}

/// Where the persistent node identity lives under the data directory.
fn identity_path(data_dir: &Path) -> PathBuf {
    data_dir.join("identities").join("lblogd")
}

/// Build the node's `nomadnetwork.node` destination from its identity.
fn blog_destination(identity: Identity) -> Result<Destination, NodeError> {
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "nomadnetwork",
        &["node"],
    )
    .map_err(|e| NodeError::Destination(e.to_string()))?;
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    Ok(dest)
}

/// Resolve the node's destination hash from the persistent identity alone,
/// without connecting to a daemon. Generates and saves the identity on first
/// use, so the hash printed before the first serve run stays valid.
pub fn resolve_destination_hash(data_dir: &Path) -> Result<DestinationHash, NodeError> {
    let identity = load_or_generate_identity(&identity_path(data_dir))?;
    Ok(*blog_destination(identity)?.hash())
}

/// Load the persistent identity from `path`, generating and saving a fresh
/// one on first run.
fn load_or_generate_identity(path: &Path) -> Result<Identity, NodeError> {
    let io_err = |source| NodeError::IdentityIo {
        path: path.display().to_string(),
        source,
    };
    if path.exists() {
        let bytes = std::fs::read(path).map_err(io_err)?;
        Identity::from_private_key_bytes(&bytes).map_err(|e| NodeError::Identity {
            path: path.display().to_string(),
            message: format!("bad identity file: {e}"),
        })
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        let id = leviculum_std::generate_identity();
        let pk = id.private_key_bytes().map_err(|e| NodeError::Identity {
            path: path.display().to_string(),
            message: format!("key export failed: {e}"),
        })?;
        std::fs::write(path, pk).map_err(io_err)?;
        Ok(id)
    }
}

/// Which request handlers a snapshot swap has to add and remove: paths in
/// `old` that `new` no longer has, and paths `new` has that `old` did not.
/// Both sorted, so a caller's log and a test read deterministically.
///
/// Extracted from [`BlogNode::apply_snapshot`] because the deregistration is
/// invisible from outside the process: with the handler leaked, a request for
/// a deleted page still reaches the node and is dropped by the page lookup in
/// `respond`, so a client sees the same timeout either way. The end-to-end
/// test in `tests/node_integ.rs` therefore passes with the deregistration loop
/// deleted (verified 2026-07-30). What leaking costs is a handler map that
/// grows with every reload and a node that accepts requests for pages it no
/// longer serves — so the decision is what gets pinned.
fn reconcile_paths(old: &Snapshot, new: &Snapshot) -> (Vec<String>, Vec<String>) {
    // Pages and files share one request-path namespace on the wire, so they
    // are reconciled together: a picture removed from the file area must be
    // deregistered exactly as a deleted post's page is.
    let old_paths = served_request_paths(old);
    let new_paths = served_request_paths(new);
    let mut gone: Vec<String> = old_paths.difference(&new_paths).cloned().collect();
    let mut added: Vec<String> = new_paths.difference(&old_paths).cloned().collect();
    gone.sort();
    added.sort();
    (gone, added)
}

/// Every request path a snapshot answers: its pages and its files.
fn served_request_paths(snapshot: &Snapshot) -> BTreeSet<String> {
    snapshot
        .pages
        .keys()
        .cloned()
        .chain(snapshot.served_file_paths())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot that has exactly the given page paths; nothing else in it
    /// matters to `reconcile_paths`.
    fn snapshot_with(paths: &[&str]) -> Snapshot {
        Snapshot {
            pages: paths
                .iter()
                .map(|p| ((*p).to_string(), Vec::new()))
                .collect(),
            ..Snapshot::default()
        }
    }

    /// A snapshot serving the given file names out of `/tmp` (never read:
    /// `reconcile_paths` only looks at names).
    fn snapshot_with_files(pages: &[&str], files: &[&str]) -> Snapshot {
        Snapshot {
            files: files
                .iter()
                .map(|name| {
                    (
                        (*name).to_string(),
                        FileEntry {
                            name: (*name).to_string(),
                            path: PathBuf::from("/tmp").join(name),
                            len: 0,
                        },
                    )
                })
                .collect(),
            ..snapshot_with(pages)
        }
    }

    #[test]
    fn a_vanished_page_is_deregistered_and_a_new_one_registered() {
        let old = snapshot_with(&["/page/index.mu", "/page/second.mu"]);
        let new = snapshot_with(&["/page/index.mu", "/page/third.mu"]);
        let (gone, added) = reconcile_paths(&old, &new);
        assert_eq!(
            gone,
            vec!["/page/second.mu"],
            "a page that vanished from the snapshot must have its handler removed"
        );
        assert_eq!(
            added,
            vec!["/page/third.mu"],
            "a page the reload added must get a handler"
        );
    }

    #[test]
    fn a_vanished_file_is_deregistered_like_a_page() {
        // Pages and files share the request-path namespace, so a picture
        // removed from the file area has to lose its handler too.
        let old = snapshot_with_files(&["/page/index.mu"], &["antenne.png", "mast.jpg"]);
        let new = snapshot_with_files(&["/page/index.mu"], &["mast.jpg", "rig.png"]);
        let (gone, added) = reconcile_paths(&old, &new);
        assert_eq!(gone, vec!["/file/antenne.png"]);
        assert_eq!(added, vec!["/file/rig.png"]);
    }

    #[test]
    fn file_metadata_is_the_map_nomadnet_sends() {
        let blob = file_metadata("antenne.png");
        let value = rmpv::decode::read_value(&mut std::io::Cursor::new(&blob[..])).unwrap();
        let rmpv::Value::Map(entries) = value else {
            panic!("metadata must be a map, got {value:?}");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.as_str(), Some("name"));
        assert_eq!(entries[0].1.as_str(), Some("antenne.png"));
    }

    #[test]
    fn an_unchanged_page_is_left_alone() {
        let pages = ["/page/index.mu", "/page/hello.mu"];
        let (gone, added) = reconcile_paths(&snapshot_with(&pages), &snapshot_with(&pages));
        assert!(
            gone.is_empty() && added.is_empty(),
            "an unchanged path must be neither churned nor dropped, got \
             gone={gone:?} added={added:?}"
        );
    }
}
