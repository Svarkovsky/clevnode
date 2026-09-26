//! `lnmsg send` against the real Python LXMF stack (`reference/LXMF`).
//!
//! Standing policy is that every feature gets interop tests against real
//! Python, positive and negative, before it merges. This is that test for the
//! send path, and it drives the shipped binary rather than the library, so
//! what is asserted is exactly what a cron job would get: an exit code, a
//! silent stdout, and a message in someone else's inbox.
//!
//! The interop claim rests on one identity: the id our `LNMSG_ENQUEUED` line
//! carries is the `message_hash` **Python** computed over the message it
//! received. Both sides derive it independently from the LXMF wire bytes, so
//! the match is agreement between two stacks and not a value we handed over.
//!
//! ```text
//!   lnmsg (subprocess)                    python3 scripts/test_daemon.py
//!         │                                    (real RNS + LXMF.LXMRouter)
//!         │ shared-instance IPC                     │
//!         v                                         │ TCP
//!   in-process Rust daemon ────────────────────────-┘
//!   (share_instance + TCP client)
//! ```
//!
//! The Rust daemon is built in-process rather than by spawning `lnsd`: it is
//! the same `ReticulumNodeBuilder` configuration `lnsd` assembles from a
//! config file, and `CARGO_BIN_EXE_` only reaches this package's own binaries.
//! What is under test is `lnmsg`, and it attaches over the same IPC socket
//! either way — the drop-in property is exercised more directly by the
//! `rnsd_interop` suites, which point the same clients at both daemons.

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::ReticulumNode;

/// Host-wide listener-port allocator, shared with the `mvr`, `rnsd_interop`
/// and `discovery_autoconnect` suites. Included by path rather than copied:
/// the module's whole point is that there is ONE counter per host, and a
/// private one here would walk the same numbers as everyone else's.
#[path = "../../leviculum-std/tests/support/port_alloc.rs"]
#[allow(dead_code)]
mod port_alloc;

const LNMSG: &str = env!("CARGO_BIN_EXE_lnmsg");

/// `scripts/test_daemon.py`, relative to this package.
fn daemon_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/test_daemon.py")
}

/// A running Python RNS daemon with an LXMF router, and its JSON-RPC port.
struct PythonPeer {
    process: std::process::Child,
    rns_port: u16,
    cmd_port: u16,
}

impl PythonPeer {
    fn start() -> Self {
        let rns_port = port_alloc::free_tcp_port();
        let cmd_port = port_alloc::free_tcp_port();
        let mut command = Command::new("python3");
        command
            .arg(daemon_script())
            .args(["--rns-port", &rns_port.to_string()])
            .args(["--cmd-port", &cmd_port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // Supervised so the Python daemon dies with this test binary rather
        // than outliving a failed run and holding its ports.
        let mut process =
            leviculum_std::process::spawn_supervised(command).expect("spawn the Python daemon");

        let stdout = process.stdout.take().expect("stdout is piped");
        let mut reader = BufReader::new(stdout);
        let mut ready = String::new();
        loop {
            ready.clear();
            let read = reader
                .read_line(&mut ready)
                .expect("read the daemon stdout");
            assert!(read > 0, "the Python daemon exited before saying READY");
            if ready.starts_with("READY ") {
                break;
            }
        }
        // The daemon falls back to a kernel-allocated command port when the
        // requested one was taken, and reports the bound one on the READY
        // line; trusting the requested number would talk to nothing.
        let bound: Vec<&str> = ready.split_whitespace().collect();
        let cmd_port = bound[2].parse::<u16>().expect("the READY line's cmd port");

        let peer = Self {
            process,
            rns_port,
            cmd_port,
        };
        assert_eq!(
            peer.rpc("ping", serde_json::json!({})),
            serde_json::json!("pong"),
            "the Python daemon must answer on its command port"
        );
        peer
    }

    /// One JSON-RPC call: connect, write, half-close, read the answer.
    fn rpc(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::net::TcpStream;
        let mut stream = TcpStream::connect(("127.0.0.1", self.cmd_port))
            .expect("connect to the Python command port");
        let request = serde_json::json!({ "method": method, "params": params });
        stream
            .write_all(request.to_string().as_bytes())
            .expect("write the command");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("half-close");
        let mut response = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut response).expect("read the answer");
        let response: serde_json::Value =
            serde_json::from_slice(&response).expect("the answer is JSON");
        assert!(
            response.get("error").is_none(),
            "{method} failed: {response}"
        );
        response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    /// Bring up the Python LXMF router and return its delivery address.
    fn lxmf_init(&self) -> String {
        self.rpc("lxmf_init", serde_json::json!({}))
            .get("delivery_hash")
            .and_then(|v| v.as_str())
            .expect("lxmf_init returns a delivery hash")
            .to_string()
    }

    fn lxmf_announce(&self) {
        self.rpc("lxmf_announce", serde_json::json!({}));
    }

    fn received(&self) -> Vec<serde_json::Value> {
        match self.rpc("lxmf_get_received", serde_json::json!({})) {
            serde_json::Value::Array(items) => items,
            _ => Vec::new(),
        }
    }
}

impl Drop for PythonPeer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// The Rust daemon `lnmsg` attaches to, plus everything it needs alive.
struct Mesh {
    daemon: ReticulumNode,
    instance: String,
    home: tempfile::TempDir,
    _storage: tempfile::TempDir,
}

impl Mesh {
    async fn start(python_rns_port: u16) -> Self {
        // Unique per process AND per call: two tests in one binary run in
        // parallel threads and would otherwise fight over one abstract socket.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let instance = format!(
            "lnmsg-interop-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let storage = tempfile::tempdir().expect("daemon storage");
        let python: SocketAddr = format!("127.0.0.1:{python_rns_port}")
            .parse()
            .expect("loopback address");

        let mut daemon = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .share_instance(true)
            .instance_name(instance.clone())
            .add_tcp_client(python)
            .storage_path(storage.path().to_path_buf())
            .build_sync()
            .expect("build the Rust daemon");
        daemon.start().await.expect("start the Rust daemon");
        // Let the abstract Unix socket listener and the TCP link come up
        // before a client connects; the same wait `lnomad`'s integ test uses.
        tokio::time::sleep(Duration::from_millis(500)).await;

        Self {
            daemon,
            instance,
            home: tempfile::tempdir().expect("lnmsg state dir"),
            _storage: storage,
        }
    }

    /// Run `lnmsg send` against this mesh, on a blocking thread so the daemon's
    /// runtime keeps serving the IPC socket while the child talks to it.
    ///
    /// The child writes its structured event log into the state directory and
    /// [`Sent::log`] carries it back, so a failing assertion prints the state
    /// sequence that produced it rather than a bare exit code. That is what the
    /// log is for (`docs/src/concepts/lnmsg-architecture.md` §7).
    async fn send(&self, args: Vec<String>, stdin: Vec<u8>) -> Sent {
        let instance = self.instance.clone();
        let home = self.home.path().to_path_buf();
        let log = home.join(format!("events-{}.log", args.join("-")));
        let read_log = log.clone();
        let sent = tokio::task::spawn_blocking(move || {
            let mut command = Command::new(LNMSG);
            command
                .arg("send")
                .args(&args)
                .args(["--instance", &instance])
                .env("LNMSG_HOME", &home)
                .env("LEVICULUM_EVENT_LOG", &log)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // Supervised for the same reason the Python daemon is: a client
            // that outlives a killed run keeps talking to the shared instance.
            let mut child = leviculum_std::process::spawn_supervised(command).expect("spawn lnmsg");
            child
                .stdin
                .take()
                .expect("stdin is piped")
                .write_all(&stdin)
                .expect("write the body");
            let output = child.wait_with_output().expect("wait for lnmsg");
            Sent {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                log: String::new(),
            }
        })
        .await
        .expect("the lnmsg child thread");
        Sent {
            log: std::fs::read_to_string(&read_log).unwrap_or_default(),
            ..sent
        }
    }
}

/// One `lnmsg send` run, as a script and a debugger see it.
struct Sent {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// The child's structured event log, for failure messages.
    log: String,
}

impl Sent {
    /// The message id, from the `id=` field of this run's `LNMSG_ENQUEUED`
    /// line. Since 2026-08-21 the id is not on stdout, and this is the
    /// documented way to get it back out of a run.
    fn enqueued_id(&self) -> String {
        let line = self
            .log
            .lines()
            .find(|line| line.starts_with("LNMSG_ENQUEUED "))
            .unwrap_or_else(|| panic!("no LNMSG_ENQUEUED line in this run: {self}"));
        let id = line
            .split_whitespace()
            .find_map(|token| token.strip_prefix("id="))
            .unwrap_or_else(|| panic!("no id= field in {line}"));
        assert_eq!(
            id.len(),
            64,
            "an LXMF message id is 32 bytes of hex: {id:?}"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "the id must be hex: {id:?}"
        );
        id.to_string()
    }
}

impl std::fmt::Display for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {:?}\nstdout: {:?}\nstderr: {}\nevents:\n{}",
            self.code, self.stdout, self.stderr, self.log
        )
    }
}

/// Poll the Python router until it has delivered the message with this id.
async fn wait_for_python(peer: &PythonPeer, id: &str, within: Duration) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let seen = peer.received();
        if let Some(message) = seen
            .iter()
            .find(|m| m.get("message_hash").and_then(|v| v.as_str()) == Some(id))
        {
            return message.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("Python never received {id}; it holds: {seen:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn hex_bytes(message: &serde_json::Value, key: &str) -> Vec<u8> {
    hex::decode(
        message
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{key} is missing from {message}")),
    )
    .unwrap_or_else(|_| panic!("{key} is not hex in {message}"))
}

/// Positive: a message sent by the shipped binary arrives in a real Python
/// LXMF inbox with its body and title intact, non-ASCII included.
#[tokio::test(flavor = "multi_thread")]
async fn lnmsg_send_reaches_a_python_lxmf_receiver() {
    let peer = PythonPeer::start();
    let python_address = peer.lxmf_init();
    let mut mesh = Mesh::start(peer.rns_port).await;
    peer.lxmf_announce();

    // Non-ASCII in both body and title: LXMF carries bytes, and a messenger
    // that mangles them is useless to everyone outside en_US.
    let body = "Grüße vom Dachboden — 91 % voll ☂";
    let title = "Füllstand";

    let sent = mesh
        .send(
            vec![
                python_address.clone(),
                "--title".to_string(),
                title.to_string(),
                "--timeout".to_string(),
                "40".to_string(),
            ],
            format!("{body}\n").into_bytes(),
        )
        .await;

    assert_eq!(sent.code, Some(0), "{sent}");
    // Byte-exact, not "contains no hex blob": an assertion that merely failed
    // to find an id would pass for any other thing we started printing.
    assert_eq!(
        sent.stdout, "",
        "a successful send says nothing on stdout: {sent}"
    );

    // The id from our own event log; `wait_for_python` accepts it only if it
    // equals the `message_hash` Python computed over what it received.
    let id = sent.enqueued_id();
    let message = wait_for_python(&peer, &id, Duration::from_secs(40)).await;
    assert_eq!(
        String::from_utf8(hex_bytes(&message, "content")).expect("the body round-trips as UTF-8"),
        body,
        "the body must arrive byte for byte"
    );
    assert_eq!(
        String::from_utf8(hex_bytes(&message, "title")).expect("the title round-trips as UTF-8"),
        title
    );
    assert_eq!(
        message.get("destination_hash").and_then(|v| v.as_str()),
        Some(python_address.as_str())
    );
    // lnmsg announces its own delivery destination before sending precisely so
    // this holds: without the announce the reference marks our message
    // SOURCE_UNKNOWN and nobody could reply to us.
    assert_eq!(
        message.get("signature_validated").and_then(|v| v.as_bool()),
        Some(true),
        "Python must be able to validate our signature: {message}"
    );

    // The name a recipient actually sees. Python resolves it out of the
    // announce app_data the way its clients do, so this is the only check that
    // covers the whole path: resolution, announce, wire, reference decoder.
    // The expected value is resolved here by the same code the child ran,
    // rather than hardcoded, because the account name differs per host.
    let expected_name = lnmsg::display_name::from_process(None)
        .expect("no override is set for the test process")
        .name;
    assert_eq!(
        message.get("source_display_name").and_then(|v| v.as_str()),
        Some(expected_name.as_str()),
        "Python must see the operator's name, not the tool's: {message}"
    );
    assert_ne!(
        message.get("source_display_name").and_then(|v| v.as_str()),
        Some("lnmsg"),
        "'lnmsg' names the program, not the person it sends for"
    );

    // And `--from` is what an operator uses when the bare account name is
    // ambiguous — three hosts, one user. Same identity, so this run's announce
    // replaces the name Python holds for us.
    let overridden = mesh
        .send(
            vec![
                python_address.clone(),
                "--from".to_string(),
                "lew@schneckenschreck".to_string(),
                "--timeout".to_string(),
                "40".to_string(),
            ],
            b"second line\n".to_vec(),
        )
        .await;
    assert_eq!(overridden.code, Some(0), "{overridden}");
    assert_eq!(
        overridden.stdout, "",
        "a successful send says nothing on stdout: {overridden}"
    );
    let overridden_id = overridden.enqueued_id();
    let message = wait_for_python(&peer, &overridden_id, Duration::from_secs(40)).await;
    assert_eq!(
        message.get("source_display_name").and_then(|v| v.as_str()),
        Some("lew@schneckenschreck"),
        "--from must reach the receiver verbatim: {message}"
    );

    mesh.daemon.stop().await.expect("stop the Rust daemon");
}

/// Negative: an address that exists nowhere in the mesh must not produce a
/// success exit — a script that trusted exit 0 here would report a status line
/// nobody ever received. Silence alone means nothing now that a success is
/// silent too; the exit code is the whole signal, so this asserts it exactly.
#[tokio::test(flavor = "multi_thread")]
async fn a_destination_that_does_not_exist_is_not_a_success() {
    let peer = PythonPeer::start();
    peer.lxmf_init();
    let mut mesh = Mesh::start(peer.rns_port).await;
    peer.lxmf_announce();

    // Well-formed, and belonging to nobody: same length and shape as the real
    // one above, so what is being tested is reachability and not parsing.
    let nowhere = "0123456789abcdef0123456789abcdef";

    let sent = mesh
        .send(
            vec![
                nowhere.to_string(),
                "--timeout".to_string(),
                "8".to_string(),
            ],
            b"this goes nowhere\n".to_vec(),
        )
        .await;

    assert_ne!(
        sent.code,
        Some(0),
        "an unreachable address is not a success"
    );
    assert_eq!(sent.code, Some(1), "{sent}");
    assert_eq!(sent.stdout, "", "stdout stays empty on failure too: {sent}");
    assert!(
        sent.stderr.contains(nowhere),
        "the error must name the address it could not reach: {sent}"
    );
    assert!(
        peer.received().is_empty(),
        "nothing may have reached the Python router"
    );

    mesh.daemon.stop().await.expect("stop the Rust daemon");
}
