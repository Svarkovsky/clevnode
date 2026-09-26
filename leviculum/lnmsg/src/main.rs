//! `lnmsg` binary: the non-interactive send path of the terminal LXMF
//! messenger described in `docs/src/concepts/lnmsg.md`.
//!
//! ```sh
//! echo "disk 91%" | lnmsg send <address> --title "backup"
//! ```
//!
//! It attaches to a running `lnsd` or `rnsd` shared instance the way `lnomad`
//! does, queues one message through its own LXMF router and exits. A success
//! says nothing at all; errors go to stderr. It never starts a Reticulum stack
//! of its own, and it never claims a message was delivered.
//!
//! Exit codes follow `lnomad`'s convention (`lnomad/src/main.rs:174`, `:188`,
//! `:222`): 0 success, 1 operational failure, 2 argument error.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use leviculum_std::config::Config;

use lnmsg::engine::{attach, AttachConfig};
use lnmsg::outbox::{SendRequest, Via};
use lnmsg::send::{run_send, SendOptions};
use lnmsg::{address, body, display_name, events, identity};

/// Everything after attaching gets this many seconds by default: becoming
/// ready, learning a route to the destination, queueing, and getting the
/// message onto the network. A cron job needs to know when the command is
/// guaranteed to be gone more than it needs a slow path to succeed.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Parser, Debug)]
#[command(
    name = "lnmsg",
    version = env!("LEVICULUM_VERSION"),
    about = "Send LXMF messages over Reticulum",
    long_about = "Send LXMF messages over a running Reticulum shared instance.\n\n\
                  lnmsg attaches to a daemon that is already running (lnsd, or \
                  Python's rnsd) the same way lnomad does. It does not start a \
                  Reticulum stack of its own, so a daemon has to be running.\n\n\
                  A send that worked prints nothing and exits 0, which means \
                  the message was queued cleanly -- not that it was delivered. \
                  Failures explain themselves on stderr. Set \
                  LEVICULUM_EVENT_LOG=<path> for the full run as structured \
                  events, including the message id."
)]
struct Args {
    #[command(subcommand)]
    command: SubCommand,
}

#[derive(Subcommand, Debug)]
enum SubCommand {
    /// Queue one message for an LXMF address.
    Send(SendArgs),
}

#[derive(clap::Args, Debug)]
struct SendArgs {
    /// Recipient's LXMF address: 32 hex characters, with or without the
    /// `lxmf@` prefix lnomad copies to the clipboard.
    address: String,

    /// Message body. Omitted, or given as `-`, the body is read from stdin —
    /// which is the form a script uses: `echo "disk 91%" | lnmsg send <addr>`.
    /// One trailing newline is removed.
    body: Option<String>,

    /// Message title (LXMF carries one; it may be empty).
    #[arg(long)]
    title: Option<String>,

    /// The name recipients see as the sender. Defaults to the account name of
    /// the user running lnmsg; `LNMSG_DISPLAY_NAME` sets it where there is no
    /// command line to edit, and this flag wins over it.
    #[arg(long, value_name = "NAME")]
    from: Option<String>,

    /// How the message travels. `propagated` (through a mailbox node) is not
    /// built yet and is refused rather than quietly turned into something else.
    #[arg(long, value_enum, default_value_t = ViaArg::Direct)]
    via: ViaArg,

    /// Shared-instance name to connect to (overrides the config file's).
    #[arg(long)]
    instance: Option<String>,

    /// Reticulum config directory (default: the platform default, like `lncp`).
    /// Only its instance name is read; lnmsg keeps its own state elsewhere.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Seconds to spend, after connecting, on getting the message onto the
    /// network. This is not a wait for delivery: the command returns as soon
    /// as the stack has taken the message.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,
}

/// The `--via` choice, a clap-facing mirror of [`Via`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ViaArg {
    /// Straight to the recipient over the mesh.
    Direct,
    /// Through a propagation node's mailbox. Not built yet.
    Propagated,
}

impl From<ViaArg> for Via {
    fn from(arg: ViaArg) -> Self {
        match arg {
            ViaArg::Direct => Via::Direct,
            ViaArg::Propagated => Via::Propagated,
        }
    }
}

/// Usage error: the command line was wrong.
const EXIT_USAGE: u8 = 2;
/// Operational failure: the command line was fine and the send was not.
const EXIT_FAILURE: u8 = 1;

fn usage(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("lnmsg: {message}");
    events::done(None, "usage", EXIT_USAGE);
    ExitCode::from(EXIT_USAGE)
}

fn failure(message: impl std::fmt::Display, outcome: &str) -> ExitCode {
    eprintln!("lnmsg: {message}");
    events::done(None, outcome, EXIT_FAILURE);
    ExitCode::from(EXIT_FAILURE)
}

/// The instance name decides the abstract socket (`\0rns/{name}`) the daemon
/// listens on, so it comes from the daemon's own config file. Same derivation
/// `lncp`, `lnstatus` and `leviculum-lxmf-node` use.
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
    let args = Args::parse();
    // `LEVICULUM_EVENT_LOG=<path>` turns the structured events into an
    // append-only file in the documented format; without it this is the plain
    // fmt subscriber and the events cost a filtered-out call. `warn` rather
    // than `info` as the default: a successful run says nothing on any stream,
    // so an `info`-level line on stderr would be the only thing a cron job
    // ever saw from a send that worked. Whoever wants the detail asks for the
    // event log, which is where the message id lives.
    leviculum_std::event_log::install_global_subscriber("warn");

    match args.command {
        SubCommand::Send(send) => run(send).await,
    }
}

async fn run(args: SendArgs) -> ExitCode {
    let destination = match address::parse(&args.address) {
        Ok(destination) => destination,
        Err(error) => return usage(format!("{}: {error}", args.address.trim())),
    };
    let via: Via = args.via.into();
    if via == Via::Propagated {
        // Refused up front and by name, rather than silently delivered
        // directly: a script that asked for a mailbox and got a direct
        // delivery would believe an offline recipient had been reached.
        return usage(
            "--via propagated is not built yet: lnmsg cannot use a propagation node.\n  \
             Leave --via at direct, or wait for the mailbox slice.",
        );
    }
    // Before the body is read: an unusable name is an argument error, and a
    // script piping into us should hear about it without first having its
    // stdin consumed.
    let display_name = match display_name::from_process(args.from.as_deref()) {
        Ok(resolved) => resolved,
        Err(error) => return usage(error),
    };
    events::sender(&display_name.name, display_name.source.as_str());
    let body = match body::resolve(args.body.as_deref(), &mut std::io::stdin().lock()) {
        Ok(body) => body,
        Err(error) => return usage(error),
    };
    let title = args.title.unwrap_or_default().into_bytes();

    let home = match identity::home_dir() {
        Ok(home) => home,
        Err(error) => return failure(error, "no-home"),
    };
    let identity = match identity::load_or_create(&home.join("identity")) {
        Ok(identity) => identity,
        Err(error) => return failure(error, "no-identity"),
    };

    let config_dir = args.config.unwrap_or_else(Config::default_config_dir);
    let instance = args.instance.unwrap_or_else(|| instance_name(&config_dir));

    let mut attached = match attach(AttachConfig {
        instance: instance.clone(),
        storage_dir: home.join("storage"),
        identity,
        display_name: display_name.name.into_bytes(),
    })
    .await
    {
        Ok(attached) => attached,
        Err(error) => return failure(error, "no-daemon"),
    };

    let options = SendOptions::new(instance, Duration::from_secs(args.timeout));
    let request = SendRequest {
        destination,
        title,
        body,
        via,
    };
    let outcome = run_send(&mut attached.outbox, request, &options).await;

    let code = match outcome {
        Ok(queued) => {
            events::done(Some(&queued.message_id), "queued", 0);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("lnmsg: {error}");
            events::done(None, "failed", EXIT_FAILURE);
            ExitCode::from(EXIT_FAILURE)
        }
    };
    attached.stop().await;
    code
}
