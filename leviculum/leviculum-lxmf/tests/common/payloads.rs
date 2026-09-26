//! Named payload generators for the lock-budget timing harnesses.
//!
//! A lock-budget number is a statement about a compressor, so a table of them
//! is only readable if it says which bytes were compressed. These three
//! generators are the columns of the tables in
//! `docs/src/concepts/core-lock-budget.md`, and they are defined once here so
//! the `leviculum-lxmf` unit harness (`measure_send_lock_costs`) and the
//! router harness (`measure_deferred_tick_costs`) cannot drift into measuring
//! different bytes under the same column name. Both include this file by
//! `#[path]`; it is not a `tests/` binary and not part of `common/mod.rs`,
//! which the vector tests use.
//!
//! Every generator is deterministic: same length in, same bytes out, on every
//! machine and every run. A documented number that cannot be reproduced is
//! not a measurement.

/// One xorshift32 step. Deterministic, and cheap enough that filling a MiB
/// costs nothing next to the build being timed.
fn xorshift(mut state: u32) -> u32 {
    state ^= state << 13;
    state ^= state >> 17;
    state ^= state << 5;
    state
}

/// Pseudo-random bytes: bz2 finds no structure to exploit, so the build pays
/// the full BWT and gets nothing back for it.
///
/// This is the honest worst case for a compressing send, and the class most
/// real large attachments fall into — anything already compressed (an image,
/// an archive, a ciphertext) looks like this to bz2.
pub fn incompressible(len: usize) -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::with_capacity(len + 4);
    let mut state: u32 = 0x9e37_79b9;
    while out.len() < len {
        state = xorshift(state);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Words a message body is actually made of. Short, repeated, with the
/// letter frequencies of prose rather than of a uniform byte source.
const WORDS: [&str; 24] = [
    "the",
    "node",
    "announced",
    "a",
    "path",
    "and",
    "transport",
    "forwarded",
    "it",
    "before",
    "link",
    "request",
    "reached",
    "us",
    "again",
    "with",
    "another",
    "resource",
    "advertisement",
    "on",
    "an",
    "interface",
    "that",
    "answers",
];

/// Message-shaped text: dictionary words in a seeded order, with sentence
/// breaks. bz2 shrinks it substantially, so the build does its full work
/// *and* succeeds — which is more work than failing, not less.
///
/// Deliberately not a single repeated token: a repeated token is
/// [`degenerate`], and the point of having both columns is that they are not
/// the same measurement.
pub fn compressible(len: usize) -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::with_capacity(len + 16);
    let mut state: u32 = 0x1234_5678;
    let mut words = 0usize;
    while out.len() < len {
        state = xorshift(state);
        out.extend_from_slice(WORDS[(state >> 11) as usize % WORDS.len()].as_bytes());
        words += 1;
        if words.is_multiple_of(11) {
            out.extend_from_slice(b".\n");
        } else {
            out.push(b' ');
        }
    }
    out.truncate(len);
    out
}

/// One byte repeated, kept as a column to document what it does rather than
/// to stand in for a message.
///
/// bz2's run-length front end collapses this before the Burrows-Wheeler
/// transform ever sees it, so a table built on it reports the cost of
/// compressing almost nothing. PR #212's harness measured only this
/// (`vec![0x5a; content_len]`) and read 2-3x below the page it was correcting.
pub fn degenerate(len: usize) -> std::vec::Vec<u8> {
    std::vec![0x5a; len]
}

/// A payload class: the column name a table prints, and the bytes behind it.
pub type Generator = fn(usize) -> std::vec::Vec<u8>;

/// The payload classes, in the order the doc prints them. Held as one array
/// so a harness cannot quietly measure two of the three.
pub const GENERATORS: [(&str, Generator); 3] = [
    ("incompressible", incompressible),
    ("compressible", compressible),
    ("degenerate", degenerate),
];
