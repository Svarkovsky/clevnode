use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use nix::pty::openpty;
use nix::sys::termios::{self, SetArg};
use tokio::sync::Mutex;
use tracing::{error, info};

use lora_proxy::control::run_control_socket;
use lora_proxy::forward::forward_kiss_frames;
use lora_proxy::pty::AsyncPty;
use lora_proxy::rules::RuleEngine;

/// KISS-aware serial proxy for LoRa fault injection testing.
///
/// Sits between a container and a real serial device (or between two
/// containers), forwarding complete KISS frames with optional fault
/// injection rules.
#[derive(Parser)]
#[command(name = "lora-proxy")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Hardware bridge: one PTY exposed to a container, one real serial port
    Hardware {
        /// Real serial device path (e.g., /dev/ttyACM0)
        #[arg(long)]
        device: PathBuf,

        /// Path where the PTY symlink will be created for the container
        #[arg(long)]
        pty_out: PathBuf,

        /// Unix socket path for the control interface
        #[arg(long)]
        control: Option<PathBuf>,
    },

    /// Virtual bridge: two PTYs, no hardware
    Virtual {
        /// PTY symlink path for side A
        #[arg(long)]
        pty_a: PathBuf,

        /// PTY symlink path for side B
        #[arg(long)]
        pty_b: PathBuf,

        /// Unix socket path for the control interface
        #[arg(long)]
        control: Option<PathBuf>,
    },
}

const SERIAL_BAUD_RATE: u32 = 115_200;

/// Created PTY pair. The slave fd is kept alive to prevent EIO on the master
/// when no external process has opened the slave yet.
struct PtyPair {
    master: OwnedFd,
    _slave: OwnedFd,
    slave_path: PathBuf,
}

fn create_pty(symlink_path: &Path) -> std::io::Result<PtyPair> {
    let pty_result =
        openpty(None, None).map_err(|e| std::io::Error::other(format!("openpty failed: {e}")))?;

    let master = pty_result.master;
    let slave = pty_result.slave;

    // Put the master into raw mode so KISS bytes pass through unmodified
    let mut termios_settings = termios::tcgetattr(&master)
        .map_err(|e| std::io::Error::other(format!("tcgetattr: {e}")))?;
    termios::cfmakeraw(&mut termios_settings);
    termios::tcsetattr(&master, SetArg::TCSANOW, &termios_settings)
        .map_err(|e| std::io::Error::other(format!("tcsetattr: {e}")))?;

    // Also set slave to raw mode
    let mut slave_termios = termios::tcgetattr(&slave)
        .map_err(|e| std::io::Error::other(format!("tcgetattr slave: {e}")))?;
    termios::cfmakeraw(&mut slave_termios);
    termios::tcsetattr(&slave, SetArg::TCSANOW, &slave_termios)
        .map_err(|e| std::io::Error::other(format!("tcsetattr slave: {e}")))?;

    // Get the slave device path
    let slave_path =
        nix::unistd::ttyname(&slave).map_err(|e| std::io::Error::other(format!("ttyname: {e}")))?;

    // Remove any existing file/symlink (including dangling symlinks that
    // exists() would return false for)
    let _ = std::fs::remove_file(symlink_path);
    if let Some(parent) = symlink_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    symlink(&slave_path, symlink_path)?;

    info!(
        slave = %slave_path.display(),
        symlink = %symlink_path.display(),
        "PTY created"
    );

    Ok(PtyPair {
        master,
        _slave: slave,
        slave_path,
    })
}

fn open_serial(device: &Path) -> std::io::Result<tokio_serial::SerialStream> {
    let builder = tokio_serial::new(device.to_string_lossy(), SERIAL_BAUD_RATE)
        .data_bits(tokio_serial::DataBits::Eight)
        .stop_bits(tokio_serial::StopBits::One)
        .parity(tokio_serial::Parity::None)
        .flow_control(tokio_serial::FlowControl::None);

    let port = tokio_serial::SerialStream::open(&builder)
        .map_err(|e| std::io::Error::other(format!("Failed to open {}: {e}", device.display())))?;

    info!(device = %device.display(), baud = SERIAL_BAUD_RATE, "Serial port opened");

    Ok(port)
}

async fn run_hardware(
    device: PathBuf,
    pty_out: PathBuf,
    control: Option<PathBuf>,
) -> std::io::Result<()> {
    let pty_pair = create_pty(&pty_out)?;
    let serial = open_serial(&device)?;
    let pty = AsyncPty::from_fd(pty_pair.master)?;

    info!(
        pty = %pty_out.display(),
        slave = %pty_pair.slave_path.display(),
        device = %device.display(),
        "Hardware bridge active"
    );

    let _slave_keepalive = pty_pair._slave;
    let engine = Arc::new(Mutex::new(RuleEngine::new()));

    if let Some(ref ctrl_path) = control {
        let engine_clone = Arc::clone(&engine);
        let ctrl = ctrl_path.clone();
        tokio::spawn(async move {
            if let Err(e) = run_control_socket(&ctrl, engine_clone).await {
                error!("Control socket error: {e}");
            }
        });
    }

    forward_kiss_frames(pty, "pty", serial, "device", engine).await
}

async fn run_virtual(
    pty_a_path: PathBuf,
    pty_b_path: PathBuf,
    control: Option<PathBuf>,
) -> std::io::Result<()> {
    let pty_pair_a = create_pty(&pty_a_path)?;
    let pty_pair_b = create_pty(&pty_b_path)?;

    let pty_a = AsyncPty::from_fd(pty_pair_a.master)?;
    let pty_b = AsyncPty::from_fd(pty_pair_b.master)?;

    info!(
        pty_a = %pty_a_path.display(),
        pty_b = %pty_b_path.display(),
        "Virtual bridge active"
    );

    let _slave_a_keepalive = pty_pair_a._slave;
    let _slave_b_keepalive = pty_pair_b._slave;
    let engine = Arc::new(Mutex::new(RuleEngine::new()));

    if let Some(ref ctrl_path) = control {
        let engine_clone = Arc::clone(&engine);
        let ctrl = ctrl_path.clone();
        tokio::spawn(async move {
            if let Err(e) = run_control_socket(&ctrl, engine_clone).await {
                error!("Control socket error: {e}");
            }
        });
    }

    forward_kiss_frames(pty_a, "pty_a", pty_b, "pty_b", engine).await
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let result = match cli.mode {
        Mode::Hardware {
            device,
            pty_out,
            control,
        } => run_hardware(device, pty_out, control).await,
        Mode::Virtual {
            pty_a,
            pty_b,
            control,
        } => run_virtual(pty_a, pty_b, control).await,
    };

    if let Err(e) = result {
        error!("Fatal: {e}");
        std::process::exit(1);
    }
}
