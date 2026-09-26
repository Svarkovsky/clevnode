//! Guards against the "silently dropped event" bug class.
//!
//! Every `NodeEvent` variant the engine can emit must be either projected to a
//! typed `LEV_EVENT_*` by `events::project()` or explicitly listed here as
//! intentionally mapped to `LEV_EVENT_OTHER`. A new engine event that is
//! neither fails this test, instead of vanishing into `LEV_EVENT_OTHER`
//! unnoticed (the class of bug that hid the missing `PathFound` projection).
//!
//! `NodeEvent` is `#[non_exhaustive]`, so the compiler cannot enforce this in
//! the FFI crate; this source-level check stands in for that, the same way
//! `guard_coverage.rs` enforces the panic-guard invariant.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Variants deliberately not given their own event type: pure observability or
/// transport-internal signals that project to `LEV_EVENT_OTHER` on purpose.
/// Add a name here (with a reason) only when exposing it would not help a C
/// application.
const INTENTIONALLY_OTHER: &[&str] = &[
    // "the transport layer already handles" path requests; informational only.
    "PathRequestReceived",
    // Channel retransmit is internal reliability observability.
    "ChannelRetransmit",
    // Interface-death frame loss is internal reliability observability.
    "FramesDropped",
    // Reports that the in-driver `CoreProcessor` panicked and was detached
    // (Codeberg #196). A `CoreProcessor` is a Rust trait registered on the
    // Rust builder; the C ABI has no way to install one, so this event cannot
    // fire for an FFI consumer and a typed code for it would be dead ABI.
    "CoreProcessorPanicked",
];

/// Variants whose `destination_hash` is deliberately not projected into
/// `dest_hash`. Add a name here (with a reason) only when a C application
/// cannot use it.
const DEST_HASH_NOT_PROJECTED: &[&str] = &[
    // Not projected at all (see INTENTIONALLY_OTHER); path requests are the
    // transport's business.
    "PathRequestReceived",
];

/// One field of a projected `NodeEvent` variant that `project()` does not carry
/// into `lev_event_t`, with the reason it does not.
struct Unprojected {
    variant: &'static str,
    field: &'static str,
    /// Why the field is absent. "not needed" is not a reason; a reason says
    /// what a C application uses *instead*, or what the field means that makes
    /// it meaningless outside the engine.
    why: &'static str,
}

/// Fields a C application does not need: the information is already reachable,
/// is derivable from something projected, or is engine-internal.
const INTENTIONALLY_UNPROJECTED: &[Unprojected] = &[
    Unprojected {
        variant: "PathFound",
        field: "hops",
        why: "queryable on demand for this destination with lev_hops_to; the \
              event's job is to say that a path exists",
    },
    Unprojected {
        variant: "LinkClosed",
        field: "is_initiator",
        why: "the same bool already reached C on this link's \
              LEV_EVENT_LINK_ESTABLISHED via lev_event_is_sender, keyed by the \
              same link_id; a second copy at close time adds nothing",
    },
    Unprojected {
        variant: "RequestReceived",
        field: "path_hash",
        why: "the truncated hash *of* `path`, which is projected verbatim and \
              readable with lev_event_path; a C app that wants the hash hashes \
              the path",
    },
    Unprojected {
        variant: "RequestReceived",
        field: "requested_at",
        why: "the requester's claimed epoch timestamp (Codeberg #164), \
              unvalidated peer input the core never does arithmetic on; a C \
              app that wants a trustworthy time timestamps on receipt",
    },
    Unprojected {
        variant: "ResourceTransferStarted",
        field: "is_sender",
        why: "the core emits this event on the receiver only (`is_sender: \
              false` at both push sites), so projecting it would hand C a \
              constant",
    },
];

/// Fields a C application *would* need but that the ABI cannot express today.
/// Each entry names what it would take to close the gap. These are defects, not
/// decisions: an entry here is a promise to add the accessor, and deleting the
/// entry is how that promise is discharged. Do not add one to silence the
/// guard — if a C app needs the field and the accessor is cheap, project it.
///
/// Empty is the correct steady state. The eight entries this register opened
/// with are all discharged (`lev_interface_stats_id` + `lev_event_interface_id`,
/// `lev_event_close_reason`, `lev_event_transfer_size`/`_data_size`,
/// `lev_event_segment_index`/`_total_segments`). Of the two left, one arrived
/// from the other list where its reason had stopped being true, and one
/// (`PacketProofRequested::peer`) waits on an ABI addition rather than on a
/// wider `project()` arm.
const PROJECTION_GAPS: &[Unprojected] = &[
    Unprojected {
        variant: "PacketProofRequested",
        field: "peer",
        why: "the peer link the proven packet arrived through (Codeberg \
              #376). A C app on a multi-peer carrier needs it to answer the \
              one neighbour that probed it instead of every live link, but \
              the field alone would be inert: the ABI's only prove call, \
              lev_send_proof, routes over the path table (which carries its \
              own via_peer) and has no ingress form at all, so the projected \
              interface_id is already unusable for proving. Closing this \
              takes a peer-aware prove entry point plus a peer field on \
              lev_event_t, i.e. the Rust NodeCore::send_proof_on_peer \
              mirrored into C — an ABI addition, not a wider project() arm",
    },
    Unprojected {
        variant: "ResourceFailed",
        field: "error",
        why: "moved here from INTENTIONALLY_UNPROJECTED, whose reason no longer \
          holds: it argued that every variant means re-request and that \
          lev_event_t has nowhere to put a discriminant. Neither is true. \
          ResourceError has 17 variants and they do not share a recovery — \
          Cancelled is deliberate, LinkClosed needs a new link first, \
          ResourceTooLarge needs a limit raised, CompressionUnsupported needs a \
          different sender — and lev_event_t now carries two discriminants \
          (close_reason, delivery_error), so the mechanism exists. Needs \
          lev_event_resource_error plus LEV_RESOURCE_ERR_* constants; not done \
          in this batch because naming a recovery for each of 17 variants is a \
          reference-checked audit, not a mechanical mapping, and a wrong \
          recovery in a doc comment is worse than no accessor",
    },
];

fn read(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Variant names declared in the core `NodeEvent` enum (4-space-indented
/// `Name {` lines between `pub enum NodeEvent {` and its closing brace).
fn node_event_variants() -> BTreeSet<String> {
    let src = read("../leviculum-core/src/node/event.rs");
    let mut variants = BTreeSet::new();
    let mut in_enum = false;
    for line in src.lines() {
        if line.contains("pub enum NodeEvent") {
            in_enum = true;
            continue;
        }
        if in_enum && line == "}" {
            break;
        }
        if !in_enum {
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent != 4 {
            continue;
        }
        if let Some(name) = trimmed.strip_suffix(" {") {
            if name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && name.chars().all(|c| c.is_ascii_alphanumeric())
            {
                variants.insert(name.to_string());
            }
        }
    }
    variants
}

/// Variant names handled by a `project()` match arm in `events.rs`.
fn projected_variants() -> BTreeSet<String> {
    let src = read("src/events.rs");
    src.lines()
        .filter_map(|line| {
            let l = line.trim_start();
            // Match-arm heads only: `NodeEvent::Name {` or `NodeEvent::Name =>`.
            let rest = l.strip_prefix("NodeEvent::")?;
            if !rest.contains('{') && !rest.contains("=>") {
                return None;
            }
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

#[test]
fn every_node_event_variant_is_projected_or_allowlisted() {
    let variants = node_event_variants();
    assert!(
        variants.len() >= 20,
        "parsed only {} NodeEvent variants — the parser likely drifted",
        variants.len()
    );

    let projected = projected_variants();
    let allow: BTreeSet<String> = INTENTIONALLY_OTHER.iter().map(|s| s.to_string()).collect();

    let mut unaccounted: Vec<String> = variants
        .iter()
        .filter(|v| !projected.contains(*v) && !allow.contains(*v))
        .cloned()
        .collect();
    unaccounted.sort();
    assert!(
        unaccounted.is_empty(),
        "NodeEvent variants neither projected nor allowlisted (they silently \
         become LEV_EVENT_OTHER): {unaccounted:?}. Either add a project() arm \
         with a LEV_EVENT_* type, or add the name to INTENTIONALLY_OTHER."
    );

    // Keep the allowlist honest: every entry must still name a real variant.
    let stale: Vec<&str> = INTENTIONALLY_OTHER
        .iter()
        .filter(|a| !variants.contains(**a))
        .copied()
        .collect();
    assert!(
        stale.is_empty(),
        "stale INTENTIONALLY_OTHER entries (no such NodeEvent variant): {stale:?}"
    );
}

/// Variant name → its field names, for the variants of `NodeEvent`.
fn node_event_fields() -> BTreeMap<String, BTreeSet<String>> {
    let src = read("../leviculum-core/src/node/event.rs");
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut in_enum = false;
    let mut current: Option<String> = None;
    for line in src.lines() {
        if line.contains("pub enum NodeEvent") {
            in_enum = true;
            continue;
        }
        if in_enum && line == "}" {
            break;
        }
        if !in_enum {
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent == 4 {
            current = trimmed
                .strip_suffix(" {")
                .filter(|n| n.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
                .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric()))
                .map(str::to_string);
            if let Some(name) = &current {
                out.entry(name.clone()).or_default();
            }
            continue;
        }
        if indent == 8 {
            if let Some(name) = &current {
                if let Some(field) = trimmed.split(':').next() {
                    let field = field.trim();
                    if !field.is_empty()
                        && field
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
                    {
                        out.entry(name.clone())
                            .or_default()
                            .insert(field.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The body of each `project()` match arm, keyed by variant name.
fn projection_arms() -> BTreeMap<String, String> {
    let src = read("src/events.rs");
    let mut out = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    for line in src.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("NodeEvent::") {
            if rest.contains('{') || rest.contains("=>") {
                if let Some((name, body)) = current.take() {
                    out.insert(name, body);
                }
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !name.is_empty() {
                    current = Some((name, String::new()));
                    continue;
                }
            }
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((name, body)) = current.take() {
        out.insert(name, body);
    }
    out
}

/// Each `project()` match arm, keyed by variant name: the fields its pattern
/// names (field name → the identifier it is bound to, `_` for a discarded
/// binding) and the arm body.
///
/// An arm runs from its `NodeEvent::Name` head to the next head, clamped at the
/// `_ =>` catch-all so the last arm does not swallow the rest of the file.
fn projection_arm_patterns() -> BTreeMap<String, (BTreeMap<String, String>, String)> {
    let src = read("src/events.rs");
    let lines: Vec<&str> = src.lines().collect();
    // Anchor on `project()` itself: the file has other matches with wildcard
    // arms (the LinkCloseReason and DeliveryError mappings), and taking the
    // file's first `_ =>` would clamp the arm scan before `project()` even
    // starts, silently reducing this guard to nothing.
    let project_start = lines
        .iter()
        .position(|l| l.contains("fn project("))
        .expect("events.rs must define project()");
    let catch_all = project_start
        + lines[project_start..]
            .iter()
            .position(|l| l.trim_start().starts_with("_ =>"))
            .expect("project() must keep its `_ =>` catch-all arm");

    let heads: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .take(catch_all)
        .skip(project_start)
        .filter_map(|(i, l)| {
            let rest = l.trim_start().strip_prefix("NodeEvent::")?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            (!name.is_empty()).then_some((i, name))
        })
        .collect();

    let mut out = BTreeMap::new();
    for (n, (start, name)) in heads.iter().enumerate() {
        let end = heads.get(n + 1).map_or(catch_all, |(i, _)| *i);
        let text = lines[*start..end].join("\n");
        let Some(arrow) = text.find("=>") else {
            continue;
        };
        let (pattern, body) = text.split_at(arrow);
        let mut fields = BTreeMap::new();
        if let (Some(open), Some(close)) = (pattern.find('{'), pattern.rfind('}')) {
            if close > open {
                for part in pattern[open + 1..close].split(',') {
                    let part = part.trim();
                    if part.is_empty() || part == ".." {
                        continue;
                    }
                    // `field` binds to itself; `field: binding` (including
                    // `field: _`) renames it.
                    let (field, binding) = match part.split_once(':') {
                        Some((f, b)) => (f.trim(), b.trim()),
                        None => (part, part),
                    };
                    if !field.is_empty()
                        && field
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
                    {
                        fields.insert(field.to_string(), binding.to_string());
                    }
                }
            }
        }
        out.insert(name.clone(), (fields, body.to_string()));
    }
    out
}

/// Does `body` mention `ident` as a whole identifier?
fn mentions(body: &str, ident: &str) -> bool {
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
    body.match_indices(ident).any(|(i, _)| {
        let before = body[..i].chars().next_back().is_none_or(boundary);
        let after = body[i + ident.len()..].chars().next().is_none_or(boundary);
        before && after
    })
}

/// The general form of the `dest_hash` check below: *every* field of a
/// projected variant must reach C or be accounted for by name.
///
/// `destination_hash` was never special — it is just the field the omission
/// happened to hit twice (Codeberg #134's `LinkEstablished`, #137's
/// `RequestReceived`). The mechanism is what generalises: an arm that binds
/// with `..` compiles clean whatever it leaves behind, and the variant-level
/// guard above only asks whether an arm exists at all, so a field added to an
/// already-projected variant reaches C only if someone remembers to widen the
/// arm.
///
/// So every field is either bound and used in its arm, or named in
/// [`INTENTIONALLY_UNPROJECTED`] (a C app does not need it) or
/// [`PROJECTION_GAPS`] (it does, and the ABI cannot say it yet). The lists are
/// data with a reason per entry, like `PKT_DROP_SUMMARY`'s reason catalog,
/// rather than a comment: a reason in a comment is not read at review time.
#[test]
fn every_field_of_a_projected_event_is_projected_or_accounted_for() {
    let fields = node_event_fields();
    let arms = projection_arm_patterns();
    assert!(
        arms.len() >= 20,
        "parsed only {} project() arms — the parser likely drifted",
        arms.len()
    );

    let mut accounted: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for e in INTENTIONALLY_UNPROJECTED.iter().chain(PROJECTION_GAPS) {
        assert!(
            !e.why.trim().is_empty(),
            "{}::{} needs a reason, not an empty string",
            e.variant,
            e.field
        );
        assert!(
            accounted.insert((e.variant, e.field), e.why).is_none(),
            "{}::{} is listed twice",
            e.variant,
            e.field
        );
    }

    let mut unaccounted = Vec::new();
    let mut bound_but_unused = Vec::new();
    for (variant, (pattern, body)) in &arms {
        let declared = fields
            .get(variant)
            .unwrap_or_else(|| panic!("project() has an arm for unknown variant {variant}"));
        // Parser drift: a pattern field that the enum does not declare means
        // one of the two parsers is reading the wrong thing.
        for f in pattern.keys() {
            assert!(
                declared.contains(f),
                "project() arm for {variant} binds `{f}`, which NodeEvent::{variant} \
                 does not declare — one of the two parsers has drifted"
            );
        }
        for field in declared {
            match pattern.get(field) {
                Some(binding) if binding != "_" => {
                    if !mentions(body, binding) {
                        bound_but_unused.push(format!("{variant}::{field}"));
                    }
                }
                // Absent (swallowed by `..`) or explicitly discarded with `_`.
                _ => {
                    if !accounted.contains_key(&(variant.as_str(), field.as_str())) {
                        unaccounted.push(format!("{variant}::{field}"));
                    }
                }
            }
        }
    }
    unaccounted.sort();
    bound_but_unused.sort();

    assert!(
        unaccounted.is_empty(),
        "fields of projected NodeEvent variants that never reach C and are not \
         accounted for: {unaccounted:?}. Either widen the project() arm, or add \
         the field to INTENTIONALLY_UNPROJECTED with the reason a C app does \
         not need it, or to PROJECTION_GAPS with the accessor it would take."
    );
    assert!(
        bound_but_unused.is_empty(),
        "fields bound by a project() arm but never used in its body: \
         {bound_but_unused:?} — they are dropped as surely as if the pattern \
         had left them to `..`."
    );

    // Keep both lists honest: an entry must still name a field of a projected
    // variant that the arm really does leave behind.
    let mut stale = Vec::new();
    for e in INTENTIONALLY_UNPROJECTED.iter().chain(PROJECTION_GAPS) {
        let Some((pattern, _)) = arms.get(e.variant) else {
            stale.push(format!(
                "{}::{} (variant is not projected)",
                e.variant, e.field
            ));
            continue;
        };
        if !fields.get(e.variant).is_some_and(|f| f.contains(e.field)) {
            stale.push(format!("{}::{} (no such field)", e.variant, e.field));
        } else if pattern.get(e.field).is_some_and(|b| b != "_") {
            stale.push(format!(
                "{}::{} (the arm does project it)",
                e.variant, e.field
            ));
        }
    }
    assert!(
        stale.is_empty(),
        "stale INTENTIONALLY_UNPROJECTED/PROJECTION_GAPS entries: {stale:?}"
    );
}

/// The `destination_hash` special case, kept alongside the general check above
/// because it pins something the general one cannot see: not just that the
/// field is bound, but that it reaches the *right* target field. A
/// `destination_hash` on an event has to land in `dest_hash` — binding it and
/// writing it somewhere else, or nowhere, leaves a responder hosting several
/// destinations unable to tell them apart (Codeberg #134's `LinkEstablished`,
/// #137's `RequestReceived`).
#[test]
fn every_projected_event_with_a_destination_sets_dest_hash() {
    let fields = node_event_fields();
    let arms = projection_arms();
    let skip: BTreeSet<String> = DEST_HASH_NOT_PROJECTED
        .iter()
        .map(|s| s.to_string())
        .collect();

    let carriers: Vec<&String> = fields
        .iter()
        .filter(|(_, f)| f.contains("destination_hash") || f.contains("destination"))
        .map(|(name, _)| name)
        .collect();
    assert!(
        carriers.len() >= 6,
        "parsed only {} destination-carrying variants — the parser likely drifted",
        carriers.len()
    );

    let mut missing: Vec<String> = carriers
        .iter()
        .filter(|name| !skip.contains(**name))
        .filter(|name| {
            arms.get(**name)
                .is_some_and(|body| !body.contains("e.dest_hash ="))
        })
        .map(|name| (*name).clone())
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "projected NodeEvent variants that carry a destination but never set \
         `e.dest_hash` (a C consumer cannot tell which destination the event \
         belongs to): {missing:?}. Either set it in the project() arm, or add \
         the name to DEST_HASH_NOT_PROJECTED with a reason."
    );

    // Keep this allowlist honest too.
    let stale: Vec<&str> = DEST_HASH_NOT_PROJECTED
        .iter()
        .filter(|a| !carriers.iter().any(|c| c.as_str() == **a))
        .copied()
        .collect();
    assert!(
        stale.is_empty(),
        "stale DEST_HASH_NOT_PROJECTED entries (no such destination-carrying \
         variant): {stale:?}"
    );
}

/// Variant names of a plain (fieldless) `#[non_exhaustive]` core enum: the
/// 4-space-indented bare `Name,` lines of `pub enum <name> {`.
fn plain_enum_variants(rel: &str, enum_name: &str) -> BTreeSet<String> {
    let src = read(rel);
    let header = format!("pub enum {enum_name} {{");
    let mut variants = BTreeSet::new();
    let mut in_enum = false;
    for line in src.lines() {
        if line.contains(&header) {
            in_enum = true;
            continue;
        }
        if in_enum && line == "}" {
            break;
        }
        if !in_enum {
            continue;
        }
        let trimmed = line.trim_start();
        if line.len() - trimmed.len() != 4 {
            continue;
        }
        if let Some(name) = trimmed.strip_suffix(',') {
            if name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && name.chars().all(|c| c.is_ascii_alphanumeric())
            {
                variants.insert(name.to_string());
            }
        }
    }
    variants
}

/// The two engine discriminants the ABI now exports by name must stay exported
/// by name.
///
/// `LinkCloseReason` and `DeliveryError` are both `#[non_exhaustive]`, so their
/// mapping functions in `events.rs` need a wildcard arm to compile, and that
/// wildcard is a bucket a new engine variant would fall into silently — the same
/// mechanism as `..` in a `project()` pattern, one level down. `LEV_CLOSE_OTHER`
/// and `LEV_DELIVERY_OTHER` exist for forward compatibility with a *peer* built
/// against a newer engine, not as a resting place for our own variants, so every
/// declared variant must have its own arm above the wildcard.
#[test]
fn every_close_reason_has_a_c_constant() {
    let src = read("src/events.rs");
    for (rel, enum_name, alias, min) in [
        (
            "../leviculum-core/src/link/mod.rs",
            "LinkCloseReason",
            "R",
            7,
        ),
        (
            "../leviculum-core/src/node/event.rs",
            "DeliveryError",
            "E",
            3,
        ),
    ] {
        let variants = plain_enum_variants(rel, enum_name);
        assert!(
            variants.len() >= min,
            "parsed only {} {enum_name} variants ({:?}) — the parser likely drifted",
            variants.len(),
            variants
        );
        let mut missing: Vec<&String> = variants
            .iter()
            .filter(|v| !src.contains(&format!("{alias}::{v} =>")))
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "{enum_name} variants with no arm in the events.rs mapping (they fall \
             into the wildcard and reach C as LEV_*_OTHER, which is reserved for \
             a newer peer's variants, not ours): {missing:?}"
        );
    }
}
