//! The two-helper loopback harness, shared by `two_node_loopback.rs` and
//! `deferred_resource_builds.rs`.
//!
//! Two `lxmf-node` helpers, in process, over TCP loopback: everything a
//! periculum scenario drives through docker, minus the containers and the
//! daemon in the middle. See `two_node_loopback.rs` for what that trade
//! covers and what it deliberately leaves to the scenarios.

// Each integration-test binary compiles this module for itself and uses a
// different subset of it; what is dead in one binary is load-bearing in the
// other.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use leviculum_lxmf_node::processor::{
    run_build_worker, BuildJob, Emitter, HelperConfig, Input, LxmfHelperProcessor, Out,
};
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};

/// Everything a single helper needs to be driven and observed.
pub struct Helper {
    pub node: ReticulumNode,
    pub inputs: Sender<Input>,
    pub lines: Receiver<Out>,
    /// Every `EVENT` line seen so far, parsed. Never drained: several
    /// assertions are "did this ever happen", which a consuming read cannot
    /// answer.
    pub events: Vec<Event>,
    /// Every stderr diagnostic seen so far, for the assertions that need to
    /// see the deferred path run (or prove it did not).
    pub logs: Vec<String>,
    /// The build-job queue, when the setup kept it out of a worker's hands
    /// ([`Setup::build_worker`] = false). What arrives here is exactly what
    /// the processor dispatched.
    pub builds: Option<Receiver<BuildJob>>,
    /// `lxmf_ready`'s hash, once it has arrived.
    pub delivery_hash: Option<String>,
    _storage: tempfile::TempDir,
}

/// One parsed `EVENT` line, the same shape the real driver parses into
/// (`periculum/src/lxmf.rs`, `EventLine`).
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub fields: BTreeMap<String, String>,
}

impl Event {
    pub fn parse(line: &str) -> Option<Event> {
        let mut tokens = line.split_whitespace();
        if tokens.next()? != "EVENT" {
            return None;
        }
        let name = tokens.next()?.to_string();
        let fields = tokens
            .filter_map(|token| token.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Some(Event { name, fields })
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

pub enum Wire {
    Listen(SocketAddr),
    Dial(SocketAddr),
    Alone,
}

impl Wire {
    /// Listen on `127.0.0.1:0`: the node binds the port itself and the test
    /// reads the kernel-assigned address back via [`Helper::listen_addr`], so
    /// no free-port probe races the bind (Codeberg #221).
    pub fn listen_any() -> Wire {
        Wire::Listen("127.0.0.1:0".parse().expect("loopback addr"))
    }
}

/// How one helper is to be built. [`Setup::new`] is the shape the original
/// loopback tests run: flag off, build worker running, exactly `main.rs`.
pub struct Setup {
    pub display_name: &'static str,
    pub wire: Wire,
    /// [`HelperConfig::defer_resource_builds`] — the router hands Resource
    /// builds out of its tick instead of composing them inside it.
    pub defer_resource_builds: bool,
    /// Spawn the shared build worker thread, as `main.rs` does. `false`
    /// leaves the job queue in [`Helper::builds`], so a test can observe
    /// dispatches and play the worker itself.
    pub build_worker: bool,
}

impl Setup {
    pub fn new(display_name: &'static str, wire: Wire) -> Setup {
        Setup {
            display_name,
            wire,
            defer_resource_builds: false,
            build_worker: true,
        }
    }
}

impl Helper {
    /// Build and start one helper the way `main.rs` wires the real binary,
    /// minus stdin/stdout: commands go in through [`Helper::command`], lines
    /// come out through [`Helper::drain`].
    pub async fn start(setup: Setup) -> Helper {
        let storage = tempfile::tempdir().expect("tempdir");
        let (lines_tx, lines_rx) = mpsc::channel::<Out>();
        let (inputs_tx, inputs_rx) = mpsc::channel::<Input>();
        let (builds_tx, builds_rx) = mpsc::channel::<BuildJob>();
        // The stamp and shutdown queues are unbounded senders whose receivers
        // are dropped immediately: no peer here advertises a stamp cost, and
        // the test decides when the node stops. A send on either fails rather
        // than blocks, which is exactly the behaviour a hook needs.
        let (stamps_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, _) = tokio::sync::mpsc::unbounded_channel();

        let builds = if setup.build_worker {
            let worker_inputs = inputs_tx.clone();
            std::thread::spawn(move || run_build_worker(builds_rx, worker_inputs));
            None
        } else {
            Some(builds_rx)
        };

        let processor = LxmfHelperProcessor::new(
            HelperConfig {
                display_name: setup.display_name.as_bytes().to_vec(),
                defer_resource_builds: setup.defer_resource_builds,
            },
            Emitter::new(lines_tx, Instant::now()),
            inputs_rx,
            stamps_tx,
            builds_tx,
            shutdown_tx,
        );

        let mut builder = ReticulumNodeBuilder::new()
            .enable_transport(false)
            .storage_path(storage.path().to_path_buf())
            .core_processor(processor);
        builder = match setup.wire {
            Wire::Listen(addr) => builder.add_tcp_server(addr),
            Wire::Dial(addr) => builder.add_tcp_client(addr),
            Wire::Alone => builder,
        };
        let mut node = builder.build().await.expect("build node");
        node.start().await.expect("start node");

        Helper {
            node,
            inputs: inputs_tx,
            lines: lines_rx,
            events: Vec::new(),
            logs: Vec::new(),
            builds,
            delivery_hash: None,
            _storage: storage,
        }
    }

    /// The bound address of this helper's TCP listener (a
    /// [`Wire::listen_any`] helper reports the kernel-assigned port here).
    pub fn listen_addr(&self) -> SocketAddr {
        self.node
            .tcp_listen_addrs()
            .first()
            .copied()
            .expect("helper has a TCP server listener")
    }

    pub fn command(&self, line: &str) {
        self.inputs
            .send(Input::Line(line.to_string()))
            .expect("helper input channel is open");
    }

    /// Absorb everything the helper has said since the last call.
    pub fn drain(&mut self) {
        while let Ok(out) = self.lines.try_recv() {
            match out {
                Out::Event(line) => {
                    if let Some(event) = Event::parse(&line) {
                        if event.name == "lxmf_ready" {
                            self.delivery_hash = event.field("hash").map(str::to_string);
                        }
                        self.events.push(event);
                    }
                }
                Out::Log(line) => {
                    // Kept visible: a failing run's diagnosis is usually here.
                    eprintln!("{line}");
                    self.logs.push(line);
                }
            }
        }
    }

    pub fn seen(&self, name: &str) -> bool {
        self.events.iter().any(|event| event.name == name)
    }

    pub fn find(&self, name: &str) -> Option<&Event> {
        self.events.iter().find(|event| event.name == name)
    }

    /// A received message matching the driver's own predicate: right source,
    /// right body (`periculum/src/executor.rs`, `lxmf_received_verdict`).
    pub fn received(&self, src: &str, body_b64: &str) -> Option<&Event> {
        self.events.iter().find(|event| {
            event.name == "lxmf_msg_received"
                && event.field("src") == Some(src)
                && event.field("body_b64") == Some(body_b64)
        })
    }

    /// How many stderr diagnostics contain `needle` so far.
    pub fn logs_containing(&self, needle: &str) -> usize {
        self.logs
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }
}

/// Poll both helpers until `done` holds or the deadline passes.
pub async fn pump_until<F>(a: &mut Helper, b: &mut Helper, budget: Duration, mut done: F) -> bool
where
    F: FnMut(&Helper, &Helper) -> bool,
{
    let deadline = Instant::now() + budget;
    loop {
        a.drain();
        b.drain();
        if done(a, b) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The base64 the driver would build for this body
/// (`periculum/src/executor.rs`: `B64.encode(body.as_bytes())`).
pub fn body_b64(body: &str) -> String {
    leviculum_lxmf_node::protocol::b64_encode(body.as_bytes())
}
