# lnsd(1)

## NAME

lnsd -- Reticulum network daemon

## SYNOPSIS

**lnsd** [**-c** *dir*] [**--storage** *dir*] [**-s**] [**--exampleconfig**] [**-v**...] [**-q**...]

## DESCRIPTION

**lnsd** runs the Reticulum network stack as a long-lived daemon process. It is a drop-in replacement for Python's **rnsd**. Other programs connect to it via shared instance IPC (Unix abstract socket).

On startup, **lnsd** reads `config` from the configuration directory, opens all configured interfaces, and begins routing packets. It keeps running until it receives SIGINT or SIGTERM.

Sending SIGUSR1 prints a diagnostic dump of internal state to stderr.

## OPTIONS

**-c**, **--config** *dir*
:   Path to the Reticulum configuration directory, the way `rnsd --config` takes one. The config file is `<dir>/config`. Without this option the default lookup order applies; see FILES.

**--storage** *dir*
:   Storage directory path. Defaults to `<config_dir>/storage`. Long-only on purpose: in `rnsd`, `-s` means `--service`, so the short letter stays reserved for that and the storage override is a Leviculum extension.

**-s**, **--service**
:   Declare that the daemon is running as a service, accepted for compatibility with `rnsd -s`. **lnsd** keeps logging to standard output, which journald captures; it does not redirect to a log file.

**--exampleconfig**
:   Print an example configuration to standard output and exit, like `rnsd --exampleconfig`. The output loads through **lnsd**'s own config loader.

**-v**, **--verbose**
:   Increase log verbosity. Once for debug, twice for trace.

**-q**, **--quiet**
:   Decrease log verbosity. Once for warnings only, twice for errors only.

## ENVIRONMENT

**RUST_LOG**
:   Overrides the verbosity flags. See the `tracing-subscriber` documentation for filter syntax.

## FILES

*/etc/reticulum/config*
:   System-wide configuration, used when it exists. The Debian package installs one here, which is also what lets Python clients find the running daemon without extra flags.

*~/.config/reticulum/config*
:   Per-user configuration, used when the system-wide file is absent.

*~/.reticulum/config*
:   Final fallback (INI format, same as Python Reticulum).

*<config_dir>/storage/*
:   Storage directory for identities, known destinations, and cached path state.

The three configuration directories are tried in that order, matching Python-Reticulum's own lookup.

## SIGNALS

**SIGINT**, **SIGTERM**
:   Graceful shutdown.

**SIGUSR1**
:   Dump diagnostic state to stderr.

## EXAMPLES

Start with default config and verbose logging:

    lnsd -v

Start with a custom config directory:

    lnsd --config /etc/reticulum

## SEE ALSO

**lnstest**(1), **lncp**(1), **lnstatus**(1)
