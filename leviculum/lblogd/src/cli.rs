//! Command line arguments for the `lblogd` binary.
//!
//! The binary has exactly two modes, both driven by the same config file:
//! the default serve mode (start the NomadNet node and the web server, run
//! forever) and `--print-hash` (resolve the node's persistent destination
//! and the served page paths, print them, exit). The [`Args`] struct is in
//! the library so parsing is unit-testable.

use std::path::PathBuf;

use clap::Parser;

/// Parsed `lblogd` command line.
#[derive(Parser, Debug, PartialEq, Eq)]
#[command(
    name = "lblogd",
    // build.rs, not CARGO_PKG_VERSION: the shipped string carries the
    // nightly build id and the git hash, so a bug report names the exact
    // commit that produced the binary.
    version = env!("LEVICULUM_VERSION"),
    about = "Dev blog server: Markdown posts over HTTP/HTTPS and NomadNet"
)]
pub struct Args {
    /// Path to the TOML config file.
    #[arg(long)]
    pub config: PathBuf,

    /// Resolve the node's destination hash and served page paths, print
    /// them, and exit without starting any server. Needs no running lnsd.
    #[arg(long)]
    pub print_hash: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_flag_parses_to_serve_mode() {
        let args = Args::try_parse_from(["lblogd", "--config", "/etc/lblogd.toml"]).unwrap();
        assert_eq!(args.config, PathBuf::from("/etc/lblogd.toml"));
        assert!(!args.print_hash);
    }

    #[test]
    fn print_hash_flag_parses() {
        let args = Args::try_parse_from(["lblogd", "--config", "/etc/lblogd.toml", "--print-hash"])
            .unwrap();
        assert_eq!(args.config, PathBuf::from("/etc/lblogd.toml"));
        assert!(args.print_hash);
    }

    #[test]
    fn missing_config_is_an_error() {
        let err = Args::try_parse_from(["lblogd"]).unwrap_err();
        assert!(err.to_string().contains("--config"), "{err}");
    }

    #[test]
    fn missing_config_with_print_hash_is_an_error() {
        let err = Args::try_parse_from(["lblogd", "--print-hash"]).unwrap_err();
        assert!(err.to_string().contains("--config"), "{err}");
    }
}
