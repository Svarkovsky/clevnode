//! `jl` — filter binary for structured event logs.
//!
//! Stage 7 / Codeberg #39 piece 4.  Reads `EVENT_NAME k=v ... t=N` lines
//! from stdin or file inputs, applies AND-combined filters, and emits
//! the matching lines unchanged.  Lines that don't fit the structured
//! shape (banners, free text, cargo-test output) pass through unchanged.
//!
//! See `docs/src/jl-jldiff.md` for the full reference + worked examples.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "jl",
    about = "Filter structured event-log lines (EVENT_NAME k=v ... t=N).",
    long_about = "Reads structured event-log lines from stdin or input files. \
                  Applies AND-combined filters and emits matching lines unchanged. \
                  Lines that don't fit the structured shape pass through unchanged."
)]
struct Args {
    /// Filter expression: `key=value`, `key=*`, `key=prefix*`, or
    /// `t<N` / `t>N` / `t<=N` / `t>=N`.  Repeat for AND.
    #[arg(long, action = clap::ArgAction::Append)]
    filter: Vec<String>,

    /// Shorthand for `--filter node=<name>`.
    #[arg(long)]
    node: Option<String>,

    /// Drop all events before the first one whose `EVENT_NAME` equals
    /// `<NAME>`.  The matching event is included.  At most one allowed.
    #[arg(long, action = clap::ArgAction::Append)]
    since_event: Vec<String>,

    /// Drop all events at and after the first one whose `EVENT_NAME`
    /// equals `<NAME>`.  The matching event is excluded.  At most one
    /// allowed.
    #[arg(long, action = clap::ArgAction::Append)]
    until_event: Vec<String>,

    /// Input file paths; without any, reads stdin.
    #[arg(value_name = "INPUT")]
    inputs: Vec<PathBuf>,
}

#[derive(Debug)]
enum Filter {
    Exact(String, String),
    HasKey(String),
    Prefix(String, String),
    TLt(u128),
    TGt(u128),
    TLe(u128),
    TGe(u128),
}

fn parse_filter(s: &str) -> Result<Filter, String> {
    if let Some(rest) = s.strip_prefix("t<=") {
        let n: u128 = rest
            .parse()
            .map_err(|_| format!("invalid filter '{s}': expected integer after 't<='"))?;
        return Ok(Filter::TLe(n));
    }
    if let Some(rest) = s.strip_prefix("t>=") {
        let n: u128 = rest
            .parse()
            .map_err(|_| format!("invalid filter '{s}': expected integer after 't>='"))?;
        return Ok(Filter::TGe(n));
    }
    if let Some(rest) = s.strip_prefix("t<") {
        let n: u128 = rest
            .parse()
            .map_err(|_| format!("invalid filter '{s}': expected integer after 't<'"))?;
        return Ok(Filter::TLt(n));
    }
    if let Some(rest) = s.strip_prefix("t>") {
        let n: u128 = rest
            .parse()
            .map_err(|_| format!("invalid filter '{s}': expected integer after 't>'"))?;
        return Ok(Filter::TGt(n));
    }
    let (key, val) = s.split_once('=').ok_or_else(|| {
        format!(
            "invalid filter '{s}': expected 'key=value', 'key=*', \
             'key=prefix*', 't<N', 't>N', 't<=N', or 't>=N'"
        )
    })?;
    if key.is_empty() {
        return Err(format!("invalid filter '{s}': empty key"));
    }
    if val.contains('=') {
        return Err(format!("invalid filter '{s}': value contains '='"));
    }
    if val == "*" {
        return Ok(Filter::HasKey(key.to_string()));
    }
    if let Some(prefix) = val.strip_suffix('*') {
        return Ok(Filter::Prefix(key.to_string(), prefix.to_string()));
    }
    Ok(Filter::Exact(key.to_string(), val.to_string()))
}

#[derive(Debug)]
struct Event {
    name: String,
    fields: BTreeMap<String, String>,
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
    })
}

fn is_valid_event_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
}

/// `event=X` is the only filter key that consults both the event line's
/// first token (`EVENT_NAME`) AND any explicit `event=` field — the latter
/// is how Stage-6's synthetic `EVENT_SCHEMA_VIOLATION` /
/// `EVENT_FIELD_VIOLATION` lines carry the event they refer to.  Treating
/// the two as one logical key makes `event=PKT_RX` match both a real
/// `PKT_RX ...` event AND a violation line that references it.
fn matches(filter: &Filter, ev: &Event) -> bool {
    match filter {
        Filter::Exact(k, v) => {
            if k == "event" {
                ev.name == *v || ev.fields.get(k).map(String::as_str) == Some(v.as_str())
            } else {
                ev.fields.get(k).map(String::as_str) == Some(v.as_str())
            }
        }
        Filter::HasKey(k) => {
            if k == "event" {
                true
            } else {
                ev.fields.contains_key(k)
            }
        }
        Filter::Prefix(k, p) => {
            if k == "event" {
                ev.name.starts_with(p.as_str())
                    || ev.fields.get(k).is_some_and(|v| v.starts_with(p.as_str()))
            } else {
                ev.fields.get(k).is_some_and(|v| v.starts_with(p.as_str()))
            }
        }
        Filter::TLt(n) => parse_t(ev).is_some_and(|t| t < *n),
        Filter::TGt(n) => parse_t(ev).is_some_and(|t| t > *n),
        Filter::TLe(n) => parse_t(ev).is_some_and(|t| t <= *n),
        Filter::TGe(n) => parse_t(ev).is_some_and(|t| t >= *n),
    }
}

fn parse_t(ev: &Event) -> Option<u128> {
    ev.fields.get("t").and_then(|s| s.parse().ok())
}

enum GateState {
    Searching,
    Including,
}

fn process<R: Read, W: Write>(
    input: R,
    filters: &[Filter],
    since_event: Option<&str>,
    until_event: Option<&str>,
    out: &mut W,
) -> io::Result<()> {
    let mut state = if since_event.is_some() {
        GateState::Searching
    } else {
        GateState::Including
    };

    for line in BufReader::new(input).lines() {
        let line = line?;
        let ev = parse_event(&line);

        // until-event: stop entirely on first match (event excluded)
        if let (Some(name), Some(ev)) = (until_event, &ev) {
            if ev.name == name {
                return Ok(());
            }
        }

        // since-event gating
        match state {
            GateState::Searching => match (&ev, since_event) {
                (Some(ev), Some(name)) if ev.name == name => {
                    state = GateState::Including;
                }
                _ => continue,
            },
            GateState::Including => {}
        }

        // emit
        match ev {
            Some(ev) => {
                if filters.iter().all(|f| matches(f, &ev)) {
                    writeln!(out, "{line}")?;
                }
            }
            None => {
                writeln!(out, "{line}")?;
            }
        }
    }
    Ok(())
}

fn run(args: Args) -> Result<(), String> {
    if args.since_event.len() > 1 {
        return Err("--since-event may be given at most once".to_string());
    }
    if args.until_event.len() > 1 {
        return Err("--until-event may be given at most once".to_string());
    }
    let mut filters: Vec<Filter> = args
        .filter
        .iter()
        .map(|s| parse_filter(s))
        .collect::<Result<_, _>>()?;
    if let Some(node) = args.node.as_deref() {
        filters.push(Filter::Exact("node".to_string(), node.to_string()));
    }

    let since = args.since_event.first().map(String::as_str);
    let until = args.until_event.first().map(String::as_str);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if args.inputs.is_empty() {
        process(io::stdin().lock(), &filters, since, until, &mut out)
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        for path in &args.inputs {
            let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            process(f, &filters, since, until, &mut out)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("jl: {msg}");
            ExitCode::from(2)
        }
    }
}
