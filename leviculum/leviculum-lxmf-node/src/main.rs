//! `lxmf-node` — I/O wiring only. The protocol lives in
//! [`leviculum_lxmf_node::protocol`], the stack in
//! [`leviculum_lxmf_node::processor`].
//!
//! Usage, as periculum spawns it:
//!
//! ```sh
//! docker exec -i <container> lxmf-node --config /root/.reticulum [display_name]
//! ```
//!
//! Reads commands from stdin (one per line), emits `EVENT …` lines on stdout
//! (one per line, flushed), diagnostics on stderr. Identical to
//! `periculum/assets/scripts/lxmf_node.py` at all three.
//!
//! The helper connects as a shared-instance client to the daemon running in
//! its container (`lnsd` or `rnsd` — the IPC and config-file formats are the
//! same, which is the whole point). All LXMF traffic therefore flows through
//! the daemon under test, exercising its real link / announce / delivery code
//! paths. A daemon that is not there is a hard startup failure, not a silent
//! fallback to an own transport: `connect_to_shared_instance` has no local
//! interfaces to fall back to, so `build`/`start` fails and the process exits
//! non-zero, which is what `RNS_REQUIRE_SHARED=1` buys on the Python side.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use leviculum_lxmf::CooperativeStamper;
use leviculum_lxmf_node::processor::{
    run_build_worker, BuildJob, Emitter, HelperConfig, Input, LxmfHelperProcessor, Out, Shutdown,
    StampJob,
};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::Config;

/// Where the daemon's config lives, and therefore which shared instance to
/// join. Default matches every other client tool in the tree
/// (`Config::default_config_dir`); periculum's containers put it at
/// `/root/.reticulum`.
const CONFIG_FLAG: &str = "--config";

/// Node storage for the helper's own client state (learned identities, paths).
///
/// Deliberately NOT the daemon's storage directory. `lncp` and `lnstatus`
/// share it safely because a client with `enable_transport(false)` writes no
/// paths, announces or packet hashes — this helper registers a destination and
/// learns identities, so it gets its own. `LXMF_STORAGE` is the variable
/// Python's helper already reads for the same purpose (`periculum/assets/scripts/lxmf_node.py:66`).
const STORAGE_ENV: &str = "LXMF_STORAGE";
const STORAGE_DEFAULT: &str = "/tmp/lxmf-state";

/// `argv[1]` when none is given (`periculum/assets/scripts/lxmf_node.py:65`).
const DEFAULT_DISPLAY_NAME: &str = "lxmf-test";

/// Hand outbound Resource builds to the build worker instead of running them
/// inside the router's tick, i.e. under the daemon connection's core lock
/// (Codeberg #196). Off by default, matching `RouterConfig`; periculum sets it
/// per node via `defer_resource_builds = true` on its `lxmf_start` step.
const DEFER_FLAG: &str = "--defer-resource-builds";

struct Args {
    display_name: String,
    config_dir: PathBuf,
    defer_resource_builds: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut display_name = None;
    let mut config_dir = None;
    let mut defer_resource_builds = false;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            CONFIG_FLAG => {
                config_dir = Some(PathBuf::from(
                    argv.next()
                        .ok_or_else(|| format!("{CONFIG_FLAG} needs a directory"))?,
                ));
            }
            DEFER_FLAG => defer_resource_builds = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}"));
            }
            positional => {
                if display_name.replace(positional.to_string()).is_some() {
                    return Err("only one display name may be given".into());
                }
            }
        }
    }
    Ok(Args {
        display_name: display_name.unwrap_or_else(|| DEFAULT_DISPLAY_NAME.to_string()),
        config_dir: config_dir.unwrap_or_else(Config::default_config_dir),
        defer_resource_builds,
    })
}

/// The instance name decides the abstract socket (`\0rns/{name}`) the daemon
/// listens on, so it has to come from the daemon's own config file. Same
/// derivation `lncp` and `lnstatus` use.
fn instance_name(config_dir: &std::path::Path) -> String {
    let config_file = config_dir.join("config");
    if config_file.exists() {
        if let Ok(config) = Config::load(&config_file) {
            return config.reticulum.instance_name;
        }
    }
    "default".to_string()
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("[lxmf-node] {e}");
            eprintln!("usage: lxmf-node [{CONFIG_FLAG} <dir>] [{DEFER_FLAG}] [display_name]");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[lxmf-node] {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let storage_dir =
        PathBuf::from(std::env::var_os(STORAGE_ENV).unwrap_or_else(|| STORAGE_DEFAULT.into()));
    std::fs::create_dir_all(&storage_dir)
        .map_err(|e| format!("could not create {}: {e}", storage_dir.display()))?;

    // The writer thread owns both output streams. Everything upstream of it —
    // including the hooks, which run under the core mutex — only pushes onto
    // an unbounded queue, so no line of output can ever block the node.
    let (lines_tx, lines_rx) = mpsc::channel::<Out>();
    let writer = thread::spawn(move || {
        let stdout = std::io::stdout();
        for line in lines_rx {
            match line {
                Out::Event(text) => {
                    let mut handle = stdout.lock();
                    // Flush per line: the driver reads them as they arrive and
                    // times steps off the arrival.
                    let _ = writeln!(handle, "{text}");
                    let _ = handle.flush();
                }
                Out::Log(text) => eprintln!("{text}"),
            }
        }
    });

    let emitter = Emitter::new(lines_tx, Instant::now());
    let (inputs_tx, inputs_rx) = mpsc::channel::<Input>();
    let (stamps_tx, mut stamps_rx) = tokio::sync::mpsc::unbounded_channel::<StampJob>();
    let (builds_tx, builds_rx) = mpsc::channel::<BuildJob>();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<Shutdown>();

    let instance = instance_name(&args.config_dir);
    emitter.log(format!(
        "[lxmf-node] starting display_name={} storage={} instance={instance}",
        args.display_name,
        storage_dir.display()
    ));

    let processor = LxmfHelperProcessor::new(
        HelperConfig {
            display_name: args.display_name.clone().into_bytes(),
            defer_resource_builds: args.defer_resource_builds,
        },
        emitter.clone(),
        inputs_rx,
        stamps_tx,
        builds_tx,
        shutdown_tx.clone(),
    );

    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .connect_to_shared_instance(&instance)
        .storage_path(storage_dir)
        .core_processor(processor)
        .build()
        .await
        .map_err(|e| format!("could not attach to the shared instance '{instance}': {e}"))?;
    let mut events = node
        .take_event_receiver()
        .ok_or("the node has no event receiver")?;
    node.start()
        .await
        .map_err(|e| format!("could not start the node: {e}"))?;
    emitter.log("[lxmf-node] shared-instance client connected");

    // The application side of the control plane. Two jobs: drain it so the
    // bounded channel cannot overflow, and notice the one event that means the
    // helper has silently stopped being an LXMF node.
    let event_emitter = emitter.clone();
    let event_shutdown = shutdown_tx.clone();
    let event_drain = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let leviculum_std::NodeEvent::CoreProcessorPanicked { hook } = event {
                // The driver has detached the processor for good and carries on
                // serving the mesh without it. From the driver's point of view
                // this helper is now a node that will never answer again, so
                // say so and stop rather than time out every later step.
                event_emitter.error(&format!(
                    "lxmf processor panicked in {hook} and was detached"
                ));
                let _ = event_shutdown.send(Shutdown::Quit);
            }
        }
    });

    // Proof-of-work, off the core lock. Dormant in every scenario whose peers
    // advertise no stamp cost, which is all of them today — Python's helper
    // registers with `stamp_cost=0`.
    //
    // Its own thread with its own current-thread runtime, not `tokio::spawn`:
    // mining is pure CPU that can run for minutes at a peer-chosen cost, so it
    // stays off the runtime's workers. `StampExecutor::generate` is `Send`
    // (Codeberg #203), so `tokio::spawn` would now accept it — this thread is a
    // scheduling choice, no longer a `!Send` workaround.
    let stamp_inputs = inputs_tx.clone();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                eprintln!("[lxmf-node] no runtime for the stamp executor: {e}");
                return;
            }
        };
        runtime.block_on(async move {
            while let Some(StampJob::Delivery(request)) = stamps_rx.recv().await {
                let mut executor = CooperativeStamper::cooperative(rand_core::OsRng);
                let input = match request.generate_with(&mut executor).await {
                    Ok(stamp) => Input::StampReady { request, stamp },
                    Err(e) => Input::StampFailed {
                        request,
                        detail: format!("{e:?}"),
                    },
                };
                if stamp_inputs.send(input).is_err() {
                    return;
                }
            }
        });
    });

    // Resource builds, off the core lock (Codeberg #196). A plain thread with
    // a blocking receive: the build is synchronous CPU work (2–60 ms), so
    // there is nothing to await. Deliberately NOT the stamp worker's queue —
    // a build behind a minutes-long stamp mine would delay a delivery by the
    // mine, and the two workloads share nothing but "runs off the lock".
    // Dormant unless the processor was built with `--defer-resource-builds`.
    let build_inputs = inputs_tx.clone();
    thread::spawn(move || run_build_worker(builds_rx, build_inputs));

    // stdin is a blocking read on a real thread, not a tokio task: the reader
    // must keep running while the runtime is busy, and `quit` has to be seen
    // even if the node is wedged.
    let stdin_inputs = inputs_tx;
    let stdin_shutdown = shutdown_tx;
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let quit = line.trim() == "quit";
            if stdin_inputs.send(Input::Line(line)).is_err() {
                return;
            }
            // Python stops reading after `quit` (`periculum/assets/scripts/lxmf_node.py:118-119`); the
            // processor emits `lxmf_shutdown` and signals the runtime.
            if quit {
                return;
            }
        }
        let _ = stdin_inputs.send(Input::Eof);
        // Belt and braces: if the processor is already detached, `Input::Eof`
        // reaches nobody and only this gets the process out.
        let _ = stdin_shutdown.send(Shutdown::Eof);
    });

    let reason = shutdown_rx.recv().await.unwrap_or(Shutdown::Eof);
    emitter.log(format!("[lxmf-node] shutting down ({reason:?})"));
    event_drain.abort();

    let _ = node.stop().await;
    drop(emitter);
    // The writer flushes every line the moment it arrives, so all that can be
    // outstanding here is the tail of the queue. This waits for that rather
    // than joining the thread: the other `Out` senders live inside the
    // driver's event-loop task and the aborted drain, which are dropped
    // asynchronously, and a join would be waiting on task teardown rather
    // than on output.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(writer);
    Ok(())
}
