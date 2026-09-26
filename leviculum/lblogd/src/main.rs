//! `lblogd` binary: serve the blog over Reticulum and the clearnet.
//!
//! Serve mode starts the NomadNet page node and the HTTP/HTTPS web server
//! concurrently and runs until either fails; both are daemons, so any return
//! is an error and the process exits non zero. Both read their content from
//! one shared snapshot, which SIGHUP reloads in place. `--print-hash`
//! resolves the node's persistent destination locally (no running lnsd
//! needed), prints the hash and the served page paths, and exits. All logic
//! lives in the library's `config`, `content`, `node`, and `web` modules;
//! main only wires them.

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use lblogd::cli::Args;
use lblogd::config::Config;
use lblogd::content::{load_snapshot, Reloader};
use lblogd::counter::{self, Counter};
use lblogd::node::{self, BlogNode};
use lblogd::{watcher, web};

type MainError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lblogd: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), MainError> {
    let config = Config::load(&args.config)?;
    if args.print_hash {
        return print_hash(&config);
    }
    serve(&config).await
}

/// Print the node's destination hash and the page paths it would serve,
/// then return without starting anything.
///
/// Loading the posts here is what makes this a dry run for publishing: the
/// same parse that serve mode performs, with the same errors, before anything
/// is restarted.
fn print_hash(config: &Config) -> Result<(), MainError> {
    let hash = node::resolve_destination_hash(&config.data_dir)?;
    let meta = config.blog_meta(Some(hash.to_string()));
    let snapshot = load_snapshot(&meta, &config.content_sources())?;
    println!("{hash}");
    for path in snapshot.served_paths() {
        println!("{path}");
    }
    for path in snapshot.served_file_paths() {
        println!("{path}");
    }
    Ok(())
}

/// Start the NomadNet node, then run it and the web server concurrently over
/// one shared content snapshot, with SIGHUP reloading it.
async fn serve(config: &Config) -> Result<(), MainError> {
    // Resolved before the node starts, so the web pages can name the address
    // the blog answers on. It comes from the persistent identity, so it is
    // the same hash the node goes on to announce.
    let address = node::resolve_destination_hash(&config.data_dir)?.to_string();
    let meta = config.blog_meta(Some(address));

    // A failure here is fatal: at startup there is no previous good state to
    // fall back on. Once running, a failed reload is not (see reload_task).
    let (reloader, content) = Reloader::new(meta, config.content_sources())?;
    let reloader = Arc::new(reloader);

    // Established before anything else starts, and synchronously: a broken
    // watch (an exhausted inotify limit, say) must fail the run rather than
    // leave the process quietly without the feature that was asked for, and
    // the watch has to be recording before the servers can invite changes.
    let posts_watcher = match config.watch_posts {
        true => {
            eprintln!(
                "lblogd: watching {} for changes",
                config.posts_dir.display()
            );
            Some(watcher::PostsWatcher::start_for(reloader.sources())?)
        }
        false => None,
    };

    // Opened before either server, because opening is also what resumes
    // today's partial count from the file: a restart that started serving
    // first would count its first requests into a day that then got
    // overwritten by the resumed total.
    let counter = Arc::new(match config.counter_path() {
        Some(path) => {
            eprintln!("lblogd: counting requests into {}", path.display());
            Counter::open(path)?
        }
        None => Counter::disabled(),
    });

    let blog = BlogNode::start(config.blog_node_config(), content.clone())
        .await?
        .with_counter(Arc::clone(&counter));
    eprintln!("lblogd: node destination {}", blog.destination_hash());
    for path in blog.served_paths() {
        eprintln!("lblogd: serving {path}");
    }
    for path in blog.served_file_paths() {
        eprintln!("lblogd: serving {path}");
    }

    // Both sides run forever; a clean return still means the daemon lost a
    // service, so it is treated as an error and try_join aborts the other.
    let node_task = async {
        blog.run()
            .await
            .map_err(|e| MainError::from(format!("node: {e}")))?;
        Err(MainError::from("node: daemon connection closed"))
    };
    let web_task = async {
        web::run_web(config.web_config(), content, Arc::clone(&counter))
            .await
            .map_err(|e| MainError::from(format!("web: {e}")))?;
        Err(MainError::from("web: server exited unexpectedly"))
    };
    let watch_task = async {
        let Some(watcher) = posts_watcher else {
            // Not enabled: idle forever rather than resolving, which would
            // end try_join and take the servers down with it.
            std::future::pending::<()>().await;
            unreachable!("pending never resolves");
        };
        watcher.run(Arc::clone(&reloader)).await;
        Err(MainError::from("watch: watcher stopped unexpectedly"))
    };

    let counter_task = async {
        counter::flush_loop(Arc::clone(&counter)).await;
        Err(MainError::from("counter: flush loop stopped unexpectedly"))
    };

    let servers = async {
        tokio::try_join!(
            node_task,
            web_task,
            reload_task(Arc::clone(&reloader)),
            watch_task,
            counter_task
        )
        .map(|_: ((), (), (), (), ())| ())
    };

    // A clean stop is a success, not a lost service: `systemctl stop` and
    // `systemctl restart` both arrive as SIGTERM, and without this the
    // process would be killed with the open day's count still in memory —
    // up to one FLUSH_INTERVAL of it.
    let result = tokio::select! {
        result = servers => result,
        signal = shutdown_signal() => {
            match signal {
                Ok(name) => {
                    eprintln!("lblogd: {name}, shutting down");
                    Ok(())
                }
                Err(e) => Err(MainError::from(format!("shutdown handler: {e}"))),
            }
        }
    };
    // Unconditional: a server that failed still served requests that were
    // counted, and there is no reason to drop them on the way out.
    if let Err(e) = counter.flush() {
        eprintln!("lblogd: counter: final flush failed: {e}");
    }
    result
}

/// Resolve when the service is asked to stop, naming the signal.
///
/// SIGTERM is what systemd sends; SIGINT is what a foreground development run
/// gets from Ctrl-C. Both mean the same thing here.
async fn shutdown_signal() -> Result<&'static str, std::io::Error> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let name = tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
    };
    Ok(name)
}

/// Reload the posts on every SIGHUP, for as long as the servers run.
///
/// A failed reload is logged and dropped: the previous snapshot stays live, so
/// a malformed post cannot take a running blog offline. That is the whole
/// point of reloading instead of restarting.
///
/// SIGHUP stays available with `watch_posts` on, so a reload can always be
/// forced regardless of what the filesystem did or did not report.
async fn reload_task(reloader: Arc<Reloader>) -> Result<(), MainError> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sighup = signal(SignalKind::hangup()).map_err(|e| format!("SIGHUP handler: {e}"))?;
    loop {
        // None means the signal stream ended, which cannot happen for SIGHUP
        // while the process lives; treat it as "no more reloads" and idle
        // rather than tearing down the servers.
        if sighup.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
        match reloader.reload() {
            Ok(count) => eprintln!("lblogd: SIGHUP: reloaded {count} posts"),
            Err(e) => eprintln!("lblogd: SIGHUP: reload failed, keeping previous content: {e}"),
        }
    }
}
