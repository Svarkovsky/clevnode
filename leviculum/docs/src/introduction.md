# Leviculum

Leviculum is a Rust implementation of the [Reticulum](https://reticulum.network/) network stack. It is wire-compatible with the Python reference implementation and runs on Linux, macOS, and embedded devices.

## What is Reticulum?

Reticulum is a networking stack for building resilient, encrypted mesh networks over any transport medium. It works over LoRa radios, TCP, UDP, serial links, or anything that can carry bytes. Every node gets a cryptographic identity. Every connection is end-to-end encrypted. No servers, no accounts, no infrastructure required.

## What does leviculum do?

Leviculum provides the same functionality as Python Reticulum but compiled to native code. The `lnsd` daemon is a drop-in replacement for `rnsd`. The `lncp` file transfer tool replaces `rncp`. Python CLI tools like `rnstatus`, `rnpath`, and `rnprobe` work against a running `lnsd` without modification.

The protocol core (`leviculum-core`) compiles as `no_std` with only `alloc`, so it runs on microcontrollers. The same code powers the Linux daemon, a future Android app, and embedded firmware.

## Who this manual is for

- **Daemon users** running `lnsd` alongside or instead of `rnsd`: start with the [lnsd quickstart](lnsd-quickstart.md) and the [man pages](man/lnsd.1.md).
- **Developers** embedding or extending the stack: read the [Concepts](architecture.md) part — the [Architecture overview](architecture.md) plus [Interface Isolation](concepts/interface-isolation.md), [Python-RNS Compatibility](concepts/python-rns-compatibility.md), [Identity and Forward Secrecy](concepts/identity-and-forward-secrecy.md), and [Storage and Embedding](concepts/storage-and-embedding.md).
- **Firmware flashers** putting Leviculum on nRF52 boards: see the firmware section and the [RNode protocol](rnode-protocol.md) reference.

The **Concepts** part explains the non-obvious design ideas; the appendix carries the authoritative [Reticulum](appendix/reticulum-specification.md) and [LXMF](appendix/lxmf-specification.md) specifications.

## Tools

Leviculum ships four binaries:

- **[lnsd](man/lnsd.1.md)** -- the Reticulum network daemon
- **[lnstest](man/lnstest.1.md)** -- test and diagnostics tool: integration self-test, diagnostic bundles, identity management, and interactive sessions
- **[lncp](man/lncp.1.md)** -- standalone file transfer utility (compatible with Python `rncp`)
- **[lnstatus](man/lnstatus.1.md)** -- network status tool (compatible with Python `rnstatus`)
