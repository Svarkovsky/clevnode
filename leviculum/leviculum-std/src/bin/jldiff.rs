//! `jldiff` — align-on compare binary for structured event logs.
//!
//! Stage 7 / Codeberg #39 piece 4.  Reads two event-log files, builds
//! an alignment-key tuple from `--align-on key,key,...` for each event,
//! and partitions events into LEFT_ONLY / RIGHT_ONLY / MATCHED_DIFFER /
//! MATCHED_IDENTICAL buckets.  Output is plain text, machine-greppable.
//!
//! See `docs/src/jl-jldiff.md` for the full reference + worked examples.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "jldiff",
    about = "Compare two structured event-log files by an alignment-key tuple."
)]
struct Args {
    /// Comma-separated key list to build the alignment tuple from
    /// (e.g. `event,dst,iface`).  Required.
    #[arg(long, required = true, value_name = "KEYS")]
    align_on: String,

    /// Left-side event-log file.
    #[arg(value_name = "LEFT_FILE")]
    left: PathBuf,

    /// Right-side event-log file.
    #[arg(value_name = "RIGHT_FILE")]
    right: PathBuf,
}

#[derive(Clone, Debug)]
struct Event {
    name: String,
    fields: BTreeMap<String, String>,
    raw: String,
}

fn parse_event(line: &str) -> Option<Event> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return None;
    }
    let mut tokens = trimmed.split_whitespace();
    let name = tokens.next()?;
    if !is_valid_event_name(name) {
        return None;
    }
    let mut fields = BTreeMap::new();
    for tok in tokens {
        let (k, v) = tok.split_once('=')?;
        if k.is_empty() {
            return None;
        }
        fields.insert(k.to_string(), v.to_string());
    }
    Some(Event {
        name: name.to_string(),
        fields,
        raw: trimmed.to_string(),
    })
}

fn is_valid_event_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
}

/// Look up an alignment-key value on an event.  Mirrors `jl`'s `event=`
/// special case: when the key is `event`, we accept either an explicit
/// `event=` field (synthetic violation lines) or the EVENT_NAME first
/// token (real events).
fn align_value<'a>(ev: &'a Event, key: &str) -> Option<&'a str> {
    if key == "event" {
        ev.fields
            .get("event")
            .map(String::as_str)
            .or(Some(ev.name.as_str()))
    } else {
        ev.fields.get(key).map(String::as_str)
    }
}

/// Compute the alignment tuple for an event.  Returns `Err(missing)` if
/// any align-key is absent — that event is unalignable.
fn align_tuple(ev: &Event, keys: &[String]) -> Result<Vec<String>, String> {
    let mut tuple = Vec::with_capacity(keys.len());
    for k in keys {
        match align_value(ev, k) {
            Some(v) => tuple.push(v.to_string()),
            None => return Err(k.clone()),
        }
    }
    Ok(tuple)
}

/// Read a file's events in file order.  Non-event lines are skipped —
/// jldiff is about events, not banner / free-text noise.
fn read_events(path: &PathBuf) -> Result<Vec<Event>, String> {
    let body = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(body.lines().filter_map(parse_event).collect())
}

/// Format the DIFF line for a matched pair — non-align keys whose values
/// differ, plus `t=` when it differs (which is normal — the whole point
/// of alignment is comparing non-time fields between corresponding
/// events).  Returns `None` when nothing differs (the pair is
/// MATCHED_IDENTICAL).
fn diff_line(left: &Event, right: &Event, align_keys: &[String]) -> Option<String> {
    let mut diffs: Vec<(String, String, String)> = Vec::new();

    let mut keys: Vec<&String> = left.fields.keys().chain(right.fields.keys()).collect();
    keys.sort();
    keys.dedup();

    for k in keys {
        if align_keys.iter().any(|ak| ak == k) {
            continue;
        }
        let lv = left.fields.get(k).map(String::as_str).unwrap_or("");
        let rv = right.fields.get(k).map(String::as_str).unwrap_or("");
        if lv != rv {
            diffs.push((k.clone(), lv.to_string(), rv.to_string()));
        }
    }

    // Also consider EVENT_NAME mismatch when align-keys do not include
    // `event` — this is rare (different EVENT_NAMEs aligning by other
    // keys) but reportable.
    if !align_keys.iter().any(|k| k == "event") && left.name != right.name {
        diffs.push(("event".to_string(), left.name.clone(), right.name.clone()));
    }

    if diffs.is_empty() {
        None
    } else {
        let parts: Vec<String> = diffs
            .into_iter()
            .map(|(k, l, r)| format!("{k}={l}|{r}"))
            .collect();
        Some(parts.join(" "))
    }
}

fn run(args: Args) -> Result<(), String> {
    let align_keys: Vec<String> = args
        .align_on
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if align_keys.is_empty() {
        return Err("--align-on must contain at least one key".into());
    }

    let left = read_events(&args.left)?;
    let right = read_events(&args.right)?;

    // Index right by tuple → file-ordered list of indices.  Unalignable
    // right events are tracked separately.
    let mut right_by_tuple: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
    let mut right_unalignable: Vec<(usize, String)> = Vec::new();
    for (idx, ev) in right.iter().enumerate() {
        match align_tuple(ev, &align_keys) {
            Ok(t) => right_by_tuple.entry(t).or_default().push(idx),
            Err(m) => right_unalignable.push((idx, m)),
        }
    }

    let mut paired_right: Vec<bool> = vec![false; right.len()];
    let mut pairs: Vec<(Event, Event)> = Vec::new();
    let mut left_only: Vec<(Event, Option<String>)> = Vec::new();
    let mut left_seen: HashMap<Vec<String>, usize> = HashMap::new();

    for ev in &left {
        match align_tuple(ev, &align_keys) {
            Err(missing) => {
                left_only.push((ev.clone(), Some(missing)));
            }
            Ok(tuple) => {
                let occurrence = *left_seen.entry(tuple.clone()).or_insert(0);
                left_seen.insert(tuple.clone(), occurrence + 1);
                if let Some(right_idx) = right_by_tuple
                    .get(&tuple)
                    .and_then(|v| v.get(occurrence).copied())
                {
                    paired_right[right_idx] = true;
                    pairs.push((ev.clone(), right[right_idx].clone()));
                } else {
                    left_only.push((ev.clone(), None));
                }
            }
        }
    }

    // Right-only = unpaired-aligned + unalignable; preserve right-file order.
    let unalignable_set: HashMap<usize, String> = right_unalignable.iter().cloned().collect();
    let mut right_only: Vec<(Event, Option<String>)> = Vec::new();
    for (idx, ev) in right.iter().enumerate() {
        if !paired_right[idx] {
            right_only.push((ev.clone(), unalignable_set.get(&idx).cloned()));
        }
    }

    // Partition pairs into MATCHED_DIFFER (with diff line) vs IDENTICAL.
    let mut differ: Vec<(Event, Event, String)> = Vec::new();
    let mut identical_count = 0_usize;
    for (l, r) in pairs {
        match diff_line(&l, &r, &align_keys) {
            Some(d) => differ.push((l, r, d)),
            None => identical_count += 1,
        }
    }

    // Emit
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    use std::io::Write;

    writeln!(out, "=== LEFT_ONLY ({}) ===", left_only.len()).map_err(|e| e.to_string())?;
    for (ev, missing) in &left_only {
        match missing {
            Some(k) => writeln!(out, "{} [unalignable: missing key {k}]", ev.raw),
            None => writeln!(out, "{}", ev.raw),
        }
        .map_err(|e| e.to_string())?;
    }
    writeln!(out).map_err(|e| e.to_string())?;

    writeln!(out, "=== RIGHT_ONLY ({}) ===", right_only.len()).map_err(|e| e.to_string())?;
    for (ev, missing) in &right_only {
        match missing {
            Some(k) => writeln!(out, "{} [unalignable: missing key {k}]", ev.raw),
            None => writeln!(out, "{}", ev.raw),
        }
        .map_err(|e| e.to_string())?;
    }
    writeln!(out).map_err(|e| e.to_string())?;

    writeln!(out, "=== MATCHED_DIFFER ({} pairs) ===", differ.len()).map_err(|e| e.to_string())?;
    for (l, r, d) in &differ {
        writeln!(out, "L: {}", l.raw).map_err(|e| e.to_string())?;
        writeln!(out, "R: {}", r.raw).map_err(|e| e.to_string())?;
        writeln!(out, "   DIFF: {d}").map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())?;
    }

    writeln!(out, "=== MATCHED_IDENTICAL ({identical_count} pairs) ===")
        .map_err(|e| e.to_string())?;

    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("jldiff: {msg}");
            ExitCode::from(2)
        }
    }
}
