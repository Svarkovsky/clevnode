//! The structured event log.
//!
//! `docs/src/concepts/lnmsg-architecture.md` §7 asks for one line per protocol
//! transition in the `EVENT_NAME key=val t=<ms>` form of
//! `docs/src/structured-event-logs.md`, and calls it cheap rather than
//! speculative. It is: the emit sites are `tracing::debug!` calls carrying an
//! `event` field, and `leviculum_std::event_log::install_global_subscriber`
//! turns `LEVICULUM_EVENT_LOG=<path>` into an append-only file of exactly that
//! format — the same switch `lnsd` honours, so one merged timeline can hold
//! the daemon's packets and the messenger's message states side by side.
//!
//! Each event name below has a matching entry in
//! `leviculum_std::event_log::EVENT_CATALOG`, which is what makes a missing
//! key a loud `EVENT_SCHEMA_VIOLATION` line rather than a quiet gap.

use crate::address::to_hex;

/// Make a value safe for the whitespace-tokenised parser the log format
/// assumes: no space, no `=`, nothing non-printable.
///
/// The fields that can carry text a user chose are the instance name, an
/// outcome word and the sender's display name — and an instance name comes
/// from a config file we do not control, while a display name is whatever an
/// operator typed after `--from`. Substituting rather than dropping keeps the
/// field present, which is what the schema check wants to see.
fn scalar(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '=' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The name this run will announce itself under, and which resolution step
/// produced it.
///
/// Emitted before anything can fail, because "the recipient saw the wrong
/// sender" is a complaint that arrives long after the run: the source word
/// separates a cron job that fell through to the last resort from an
/// interactive run that read `$USER`, which the message itself cannot.
pub fn sender(name: &str, source: &str) {
    tracing::debug!(
        event = "LNMSG_SENDER",
        from = %scalar(name),
        source = %scalar(source),
    );
}

/// Attached to a shared instance and registered an LXMF delivery destination.
/// `address` is our own address, i.e. what a reply would be sent to.
pub fn attached(instance: &str, address: &[u8; 16]) {
    tracing::debug!(
        event = "LNMSG_ATTACHED",
        instance = %scalar(instance),
        address = %to_hex(address),
    );
}

/// The destination became reachable: its identity is known and a path exists.
pub fn resolved(destination: &[u8; 16], waited_ms: u64) {
    tracing::debug!(
        event = "LNMSG_RESOLVED",
        dst = %to_hex(destination),
        waited_ms = waited_ms,
    );
}

/// The router accepted the message. This is the event the brief asks for as a
/// minimum, and since 2026-08-21 its `id` field is the only place the message
/// id appears: a successful run prints nothing, so `lnmsg status <id>` is
/// reachable only for someone who ran with `LEVICULUM_EVENT_LOG` set.
pub fn enqueued(message_id: &[u8; 32], destination: &[u8; 16], bytes: usize, via: &str) {
    tracing::debug!(
        event = "LNMSG_ENQUEUED",
        id = %to_hex(message_id),
        dst = %to_hex(destination),
        bytes = bytes,
        via = %scalar(via),
    );
}

/// One outbound state transition, as the router reported it.
pub fn state(message_id: &[u8; 32], state: &str) {
    tracing::debug!(
        event = "LNMSG_STATE",
        id = %to_hex(message_id),
        state = %scalar(state),
    );
}

/// The last line of a run: what happened, and the exit code that follows from
/// it. `id` is `-` when the run ended before a message existed.
pub fn done(message_id: Option<&[u8; 32]>, outcome: &str, code: u8) {
    tracing::debug!(
        event = "LNMSG_DONE",
        id = %message_id.map(|id| to_hex(id)).unwrap_or_else(|| "-".to_string()),
        outcome = %scalar(outcome),
        code = code,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_replaces_everything_the_parser_would_choke_on() {
        assert_eq!(scalar("my instance"), "my_instance");
        assert_eq!(scalar("a=b"), "a_b");
        assert_eq!(scalar("tab\there"), "tab_here");
        assert_eq!(scalar("nul\0"), "nul_");
        assert_eq!(scalar("plain-name_1"), "plain-name_1");
    }

    /// Non-ASCII is one `_` per character rather than per byte: the log is a
    /// diagnostic, and a field of fixed width per input character is easier to
    /// eyeball than one whose length depends on the encoding.
    #[test]
    fn non_ascii_is_substituted_per_character() {
        assert_eq!(scalar("Grüße"), "Gr__e");
    }
}
