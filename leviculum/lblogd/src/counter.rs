//! The per-day request counter and the file it writes.
//!
//! # What is counted, and what is not
//!
//! Not visitors. The two sides of `lblogd` can identify a caller to entirely
//! different degrees, and neither degree reaches "a person":
//!
//! - **On the mesh there is no visitor to count.** A request arrives as
//!   `NodeEvent::RequestReceived { link_id, .. }`, and a `LinkId` is a
//!   *session*, not a person: one reader browsing five pages over one link is
//!   one link, and the same reader returning tomorrow is a different link.
//!   Reticulum reveals who someone is only if they choose to identify, and a
//!   blog reader has no reason to. So this counts requests
//!   ([`Counts::mesh_requests`]) and links ([`Counts::mesh_sessions`]), and
//!   calls them that. `RequestReceived` itself carries no identity at all;
//!   the *link* may hold one if the peer sent a LINKIDENTIFY, so the node
//!   asks it per request and counts the answer separately
//!   ([`Counts::mesh_identified_requests`]) rather than inventing a distinct-
//!   identity number out of a field that does not exist.
//! - **On the web there is a peer address**, and this counter deliberately
//!   never looks at it — not on disk, not in memory, not for a moment. An
//!   address would buy a "unique visitors" number that is wrong anyway
//!   (CGNAT, rotating IPv6 privacy addresses, proxies, bots) while making a
//!   blog server a processor of personal data, and holding one requires an
//!   in-memory set that grows with the day. So the web side counts requests
//!   too, split only by whether the router had something to serve
//!   ([`Counts::web_requests`], [`Counts::web_not_found`]).
//!
//! Nothing here is named "visitors", because nothing here is visitors.
//!
//! # Which clock, and which day
//!
//! The **platform wall clock** ([`SystemTime`]), bucketed into **UTC**
//! calendar days by [`Date::from_system_time`] — the same UTC-day rule
//! [`crate::post`] already uses to date a post from its mtime, so the counter
//! and the posts agree about where midnight is. UTC has no daylight-saving
//! discontinuity, so no day is 23 or 25 hours long and no hour happens twice.
//! Every record carries `tz=UTC` so a bare date is never ambiguous later.
//!
//! This deliberately does *not* draw from `Transport::emission_secs`
//! (`docs/src/concepts/time-and-clocks.md`). That producer exists for wire
//! fields peers compare across our process lifetimes, and its clockless
//! fallback learns a timebase from any signature-valid announce in radio
//! range. A local log file is not a wire field, and a number a neighbour can
//! move is the wrong input for one.
//!
//! # What a clock jump does
//!
//! The open day never moves backwards. A record is only ever appended for the
//! open day, and it always carries that day's *full* running total, so a
//! last-record-wins reader cannot lose counts to a later, smaller record.
//! Together those two rules are what make a backwards jump harmless:
//!
//! - **Forward jump.** Ordinary rollover. The open day is flushed, then a new
//!   one starts at zero. A jump across several days leaves a gap, which is
//!   the honest shape — nothing happened on those days as far as we know.
//! - **Backwards jump.** The observation is attributed to the open day, which
//!   stays open, and [`Counts::clock_behind`] records that it happened. The
//!   already-written earlier day is never reopened, so it cannot be merged
//!   into or overwritten. A day that is slightly over-attributed is worse
//!   than nothing; a day silently rewritten to a smaller number is worse than
//!   both, and that is the one this refuses.
//! - **Backwards across a restart.** [`Counter::open`] resumes the *latest*
//!   date in the file when the clock reads earlier than that, rather than
//!   reopening an older one, so a dead RTC at boot cannot clobber the last
//!   day the previous run wrote.
//!
//! # The file format, and why this one
//!
//! Append-only `key=value` lines, one record per flush, **last record per
//! date wins**:
//!
//! ```text
//! DAY date=2026-08-07 tz=UTC mesh_requests=41 mesh_sessions=12 ...
//! ```
//!
//! - **A `kill -9` mid-write cannot lose or corrupt a previous day.** An
//!   append never seeks back over what is already there, so every earlier
//!   day is bytes that this process will not write to again. The worst a kill
//!   can do is leave a partial final line; the reader drops any trailing
//!   fragment that is not newline-terminated and every completed record
//!   survives. The single-JSON-object-rewritten-atomically alternative is
//!   also crash-safe *if* the temp file and its directory are both fsynced
//!   before and after the rename — but there the unit at risk is the whole
//!   history rather than one line, and a reader can catch the swap. Rewriting
//!   in place, the third option, is the one that actually loses days.
//! - **Readable by `awk` without a parser**, and it is this project's
//!   documented structured event shape (`CLAUDE.md`, "Structured event-log
//!   format"): `awk '$1=="DAY"'` and the fields are self-naming.
//! - **Extensible**: a new key is one more `k=v` on the line. A reader
//!   looking for `web_requests=` is unaffected by a key added beside it, and
//!   this parser ignores keys it does not know — including when
//!   [compacting](Counter::open), which rewrites the raw lines verbatim
//!   rather than re-serialising them, so a field written by a newer version
//!   is not dropped by an older one.
//! - **Bounded growth**: one useful record per day, plus one duplicate per
//!   [`FLUSH_INTERVAL`] in which anything was counted. A continuously busy
//!   blog therefore writes up to 288 records a day (~30 KiB); an idle one
//!   writes one. Every start compacts the file to one record per date, so the
//!   steady state after ten years of daily restarts is ~3650 lines, about
//!   400 KiB. A process that never restarts for a year is the worst case at
//!   roughly 10 MiB, and its next start collapses that to 365 lines.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::post::Date;

/// The default counter file name, under the config's `data_dir`.
pub const DEFAULT_FILE_NAME: &str = "counts.log";

/// The record keyword every counter line opens with.
pub const RECORD: &str = "DAY";

/// How often the open day is written out while it is being counted into.
///
/// This is a durability hedge against an *unclean* kill only: a rollover and
/// a clean shutdown both flush regardless, so the routine cases lose nothing.
/// Five minutes bounds both what a `kill -9` costs (five minutes of counts)
/// and what the duplicate records cost (288 lines a day at worst, collapsed
/// to one at the next start).
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(300);

/// The two comment lines at the top of a fresh or freshly compacted file.
const HEADER: &str = "\
# lblogd request counts. One record per line: DAY key=value ...
# Dates are UTC calendar days; a record holds that day's running total.
# The last record for a date wins. Appended, never rewritten in place.
";

/// A source of wall-clock time.
///
/// A parameter rather than a direct [`SystemTime::now`] call because the
/// interesting behaviour of this module is entirely at time boundaries —
/// a day rolling over, a clock jumping backwards — and a test that cannot
/// move the clock cannot reach any of it.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The platform wall clock, which is what a running `lblogd` uses.
pub fn system_clock() -> Clock {
    Arc::new(SystemTime::now)
}

/// Errors from opening or writing the counter file.
#[derive(Debug, Error)]
pub enum CounterError {
    /// Reading, creating or appending to the counter file failed.
    #[error("counter file {path}: {source}")]
    Io {
        /// The counter file path.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// One day's running totals. Every field is a count of things that happened,
/// and is named after exactly those things.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Page and file requests that reached the NomadNet node.
    ///
    /// A request, not a reader: one reader browsing five pages is five.
    pub mesh_requests: u64,
    /// Reticulum links established to the blog's destination.
    ///
    /// A session, not a person: one reader browsing five pages over one link
    /// is one, and the same reader tomorrow is another one.
    pub mesh_sessions: u64,
    /// Of [`mesh_requests`](Self::mesh_requests), those that arrived on a
    /// link whose peer had proven an identity.
    ///
    /// Expected to be zero: nothing about fetching a public page asks a
    /// NomadNet client to identify. It is counted rather than assumed so the
    /// zero is a measurement.
    pub mesh_identified_requests: u64,
    /// HTTP requests the blog's router answered, of any status.
    ///
    /// Includes the bots. The redirect-only listener that runs in front of
    /// HTTPS is a different router and is not counted here.
    pub web_requests: u64,
    /// Of [`web_requests`](Self::web_requests), those answered with 404.
    ///
    /// Carried separately so a reader can subtract: a scan for `/wp-login.php`
    /// is a request and should not quietly inflate a number about reading.
    pub web_not_found: u64,
    /// Observations that arrived while the wall clock read an *earlier*
    /// calendar day than the open one.
    ///
    /// Non-zero means a backwards clock jump happened and its counts were
    /// attributed to the open day rather than merged into a closed one.
    pub clock_behind: u64,
}

/// Every key this version writes, in the order it writes them, paired with
/// the accessor that reads it off a [`Counts`].
///
/// Reading is key-driven and order-independent; this list only fixes the
/// layout of what we emit, so a line stays easy to read by eye.
type Field = (&'static str, fn(&Counts) -> u64);

const KEYS: [Field; 6] = [
    ("mesh_requests", |c| c.mesh_requests),
    ("mesh_sessions", |c| c.mesh_sessions),
    ("mesh_identified_requests", |c| c.mesh_identified_requests),
    ("web_requests", |c| c.web_requests),
    ("web_not_found", |c| c.web_not_found),
    ("clock_behind", |c| c.clock_behind),
];

/// Assign one parsed `key=value` pair into `counts`, ignoring unknown keys.
///
/// Returns `false` only for a *known* key whose value does not parse, which
/// is a corrupt record rather than a record from another version.
fn assign(counts: &mut Counts, key: &str, value: &str) -> bool {
    let slot: &mut u64 = match key {
        "mesh_requests" => &mut counts.mesh_requests,
        "mesh_sessions" => &mut counts.mesh_sessions,
        "mesh_identified_requests" => &mut counts.mesh_identified_requests,
        "web_requests" => &mut counts.web_requests,
        "web_not_found" => &mut counts.web_not_found,
        "clock_behind" => &mut counts.clock_behind,
        _ => return true,
    };
    match value.parse::<u64>() {
        Ok(v) => {
            *slot = v;
            true
        }
        Err(_) => false,
    }
}

/// The counter itself: the open day, its running totals, and the file they
/// are appended to.
///
/// Shared by both servers as an `Arc`, so the counts are process-wide and one
/// file holds both sides. Every observation method is `&self` and takes the
/// lock only long enough to add one.
pub struct Counter {
    /// Where records are appended, or `None` when counting is switched off —
    /// in which case every observation is a cheap no-op.
    path: Option<PathBuf>,
    clock: Clock,
    state: Mutex<State>,
}

/// What the wall clock had done since the last look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shift {
    /// Still the open day.
    Same,
    /// A later day: the open one was closed and written.
    Rolled,
    /// An earlier day than the open one, which stays open regardless.
    Behind,
}

/// The open day and what is known about it.
struct State {
    /// The open day. Never moves backwards; see the module docs.
    date: Date,
    /// The running total for [`date`](Self::date), including whatever was
    /// resumed from the file at startup.
    counts: Counts,
    /// The totals the file already holds for [`date`](Self::date), so an
    /// unchanged day is not appended again.
    flushed: Counts,
    /// Whether the backwards-clock warning has already been printed. A dead
    /// RTC would otherwise emit one line per request for the life of the
    /// process.
    warned_behind: bool,
}

impl Counter {
    /// A counter that counts nothing and writes nothing.
    ///
    /// What both servers hold until something hands them a real one, so
    /// neither needs an `Option` on the hot path.
    pub fn disabled() -> Counter {
        Counter {
            path: None,
            clock: system_clock(),
            state: Mutex::new(State {
                date: Date::from_system_time(SystemTime::now()),
                counts: Counts::default(),
                flushed: Counts::default(),
                warned_behind: false,
            }),
        }
    }

    /// Open (or create) the counter file at `path` and resume its latest day.
    ///
    /// Three things happen here, in this order:
    ///
    /// 1. Every complete record is read. A trailing fragment with no newline
    ///    — what a `kill -9` mid-append leaves — is dropped, as is any line
    ///    that does not parse.
    /// 2. The open day is chosen as the *later* of today and the newest date
    ///    in the file, and its counts are resumed from that date's last
    ///    record. Resuming is what makes a mid-day restart continue the day
    ///    rather than restart it at zero; taking the later date is what stops
    ///    a clock that reads earlier than the file from reopening a day the
    ///    previous run already closed.
    /// 3. The file is compacted to one record per date, written to a temp
    ///    file and renamed over the original, so the duplicate records that
    ///    accumulate during a long run do not accumulate across runs. The
    ///    surviving line for each date is the raw line, copied verbatim, so a
    ///    key this version does not know is not dropped by compacting. A
    ///    compaction that fails is logged and otherwise ignored: the original
    ///    file is untouched by a failed rename, and a large file is only a
    ///    large file.
    pub fn open(path: impl Into<PathBuf>) -> Result<Counter, CounterError> {
        Counter::open_with_clock(path, system_clock())
    }

    /// [`open`](Self::open), reading time from `clock` instead of the
    /// platform's. The seam the day-boundary tests drive.
    pub fn open_with_clock(
        path: impl Into<PathBuf>,
        clock: Clock,
    ) -> Result<Counter, CounterError> {
        let path = path.into();
        let io = |source| CounterError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io)?;
        }

        let (records, total_lines) = read_records(&path)?;
        let today = Date::from_system_time(clock());
        let (date, counts) = match records.last_key_value() {
            Some((&last, (_, counts))) if last >= today => (last, *counts),
            _ => (today, Counts::default()),
        };

        // Worth a rewrite only when it removes lines; a file that is already
        // one record per date is left alone rather than churned on every
        // start.
        if total_lines > records.len() {
            if let Err(e) = compact(&path, &records) {
                eprintln!("lblogd: counter: compaction skipped: {e}");
            }
        }

        Ok(Counter {
            path: Some(path),
            clock,
            state: Mutex::new(State {
                date,
                counts,
                flushed: counts,
                warned_behind: false,
            }),
        })
    }

    /// One request served by the NomadNet node.
    ///
    /// `identified` is whether that request's link had a proven identity,
    /// which the node has to ask the link for — the request event itself
    /// carries no identity.
    pub fn mesh_request(&self, identified: bool) {
        self.record(|counts| {
            counts.mesh_requests += 1;
            if identified {
                counts.mesh_identified_requests += 1;
            }
        });
    }

    /// One Reticulum link established to the blog's destination.
    pub fn mesh_session(&self) {
        self.record(|counts| counts.mesh_sessions += 1);
    }

    /// One HTTP request answered by the blog's router. `found` is false when
    /// the answer was a 404.
    pub fn web_request(&self, found: bool) {
        self.record(|counts| {
            counts.web_requests += 1;
            if !found {
                counts.web_not_found += 1;
            }
        });
    }

    /// The open day and its running totals, for a caller that wants to read
    /// the live numbers without going through the file.
    pub fn open_day(&self) -> (Date, Counts) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.date, state.counts)
    }

    /// Write the open day out, closing any day the clock has left behind
    /// first.
    ///
    /// Called on the [`FLUSH_INTERVAL`] tick and once more at shutdown, which
    /// is what keeps a clean stop from losing the day's partial count. Writes
    /// nothing when nothing has changed since the last record.
    pub fn flush(&self) -> Result<(), CounterError> {
        if self.path.is_none() {
            return Ok(());
        }
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // The `Behind` verdict is deliberately dropped here: `clock_behind`
        // counts observations that arrived under a backwards clock, and a
        // flush is not one. Counting it would make an idle blog with a dead
        // RTC accumulate one per tick and read like traffic.
        self.roll(&mut state, now);
        self.append(&mut state, now)
    }

    /// Add one observation to the open day, rolling the day over first if the
    /// clock has moved on.
    ///
    /// An I/O failure while rolling is logged rather than propagated: a blog
    /// that stops serving pages because it could not write a counter would be
    /// a worse failure than a missing count.
    fn record(&self, add: impl FnOnce(&mut Counts)) {
        if self.path.is_none() {
            return;
        }
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.roll(&mut state, now) == Shift::Behind {
            state.counts.clock_behind += 1;
        }
        add(&mut state.counts);
    }

    /// Reconcile the open day with the clock, and say what the clock did.
    ///
    /// Forward: close the open day and start the new one at zero. Backwards:
    /// keep the open day and warn once. See the module docs for why those are
    /// not symmetric.
    fn roll(&self, state: &mut State, now: SystemTime) -> Shift {
        let today = Date::from_system_time(now);
        if today == state.date {
            return Shift::Same;
        }
        if today < state.date {
            if !state.warned_behind {
                state.warned_behind = true;
                eprintln!(
                    "lblogd: counter: wall clock reads {today}, behind the open day {}; \
                     counting into {} rather than rewriting a day already written",
                    state.date, state.date
                );
            }
            return Shift::Behind;
        }
        if let Err(e) = self.append(state, now) {
            eprintln!("lblogd: counter: could not write {}: {e}", state.date);
        }
        state.date = today;
        state.counts = Counts::default();
        state.flushed = Counts::default();
        Shift::Rolled
    }

    /// Append one record for the open day, unless the file already holds
    /// exactly these counts for it.
    fn append(&self, state: &mut State, now: SystemTime) -> Result<(), CounterError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if state.counts == state.flushed {
            return Ok(());
        }
        let io = |source| CounterError::Io {
            path: path.display().to_string(),
            source,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io)?;
        // Header and record in one write: a kill between two writes would
        // otherwise be able to leave a header with no record behind it.
        let mut buf = String::new();
        if file.metadata().map_err(io)?.len() == 0 {
            buf.push_str(HEADER);
        }
        buf.push_str(&format_record(state.date, &state.counts, now));
        file.write_all(buf.as_bytes()).map_err(io)?;
        file.sync_data().map_err(io)?;
        state.flushed = state.counts;
        Ok(())
    }
}

/// Flush the counter every [`FLUSH_INTERVAL`], forever.
///
/// Runs alongside the servers. The tick is also what closes a day on an idle
/// blog, where no request arrives to notice the rollover.
pub async fn flush_loop(counter: Arc<Counter>) {
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    tick.tick().await; // the first tick is immediate; there is nothing yet
    loop {
        tick.tick().await;
        if let Err(e) = counter.flush() {
            eprintln!("lblogd: counter: {e}");
        }
    }
}

/// One record line, newline included.
fn format_record(date: Date, counts: &Counts, written: SystemTime) -> String {
    let mut line = format!("{RECORD} date={date} tz=UTC");
    for (key, get) in KEYS {
        line.push_str(&format!(" {key}={}", get(counts)));
    }
    line.push_str(&format!(" written={}\n", utc_timestamp(written)));
    line
}

/// A `YYYY-MM-DDThh:mm:ssZ` stamp, so a record says when it was written as
/// well as which day it is about — which is how a partial day is told from a
/// finished one.
fn utc_timestamp(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        Date::from_system_time(time),
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// The last record for each date, plus how many record lines were read.
///
/// The count is what tells [`Counter::open`] whether compacting would remove
/// anything. A missing file is an empty result, not an error: the first run
/// creates it.
type Records = std::collections::BTreeMap<Date, (String, Counts)>;

fn read_records(path: &Path) -> Result<(Records, usize), CounterError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Records::new(), 0)),
        Err(source) => {
            return Err(CounterError::Io {
                path: path.display().to_string(),
                source,
            })
        }
    };
    // Lossy rather than strict: a torn write can leave bytes that are not
    // UTF-8, and one bad line must not make the whole history unreadable.
    let text = String::from_utf8_lossy(&bytes);
    let mut records = Records::new();
    let mut lines = 0;
    for line in text.split_inclusive('\n') {
        // A fragment with no newline is a half-written append. Drop it: the
        // record it was becoming is the one thing a kill is allowed to cost.
        if !line.ends_with('\n') {
            continue;
        }
        if let Some((date, counts)) = parse_record(line) {
            lines += 1;
            records.insert(date, (line.trim_end().to_string(), counts));
        }
    }
    Ok((records, lines))
}

/// Parse one record line, or `None` if it is not one.
///
/// Unknown keys are ignored and absent keys are zero, which together are the
/// extensibility rule: a file written by a newer `lblogd` still reads here,
/// and one written by an older one does not need its missing fields invented.
fn parse_record(line: &str) -> Option<(Date, Counts)> {
    let mut fields = line.split_whitespace();
    if fields.next()? != RECORD {
        return None;
    }
    let mut date = None;
    let mut counts = Counts::default();
    for field in fields {
        let (key, value) = field.split_once('=')?;
        match key {
            "date" => date = Some(value.parse::<Date>().ok()?),
            "tz" | "written" => {}
            _ => {
                if !assign(&mut counts, key, value) {
                    return None;
                }
            }
        }
    }
    Some((date?, counts))
}

/// Rewrite `path` as the header plus one line per date, atomically.
///
/// Temp file, fsync, rename, fsync the directory: the original stays whole
/// until the rename, so a crash anywhere in here loses the compaction and
/// nothing else. The lines are the raw lines read back, so compacting cannot
/// drop a field this version does not know about.
fn compact(path: &Path, records: &Records) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    let mut buf = String::from(HEADER);
    for (line, _) in records.values() {
        buf.push_str(line);
        buf.push('\n');
    }
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(buf.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        // Without this the rename itself can be lost to a power cut, leaving
        // both names pointing at the old inode or at neither.
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock a test can move, and the [`Clock`] reading it.
    #[derive(Clone)]
    struct TestClock(Arc<Mutex<SystemTime>>);

    impl TestClock {
        /// Start at midnight UTC of `day` days after the epoch.
        fn at_day(day: u64) -> TestClock {
            TestClock(Arc::new(Mutex::new(
                UNIX_EPOCH + Duration::from_secs(day * 86_400),
            )))
        }

        fn clock(&self) -> Clock {
            let cell = Arc::clone(&self.0);
            Arc::new(move || *cell.lock().unwrap())
        }

        fn set_day(&self, day: u64, hour: u64) {
            *self.0.lock().unwrap() = UNIX_EPOCH + Duration::from_secs(day * 86_400 + hour * 3600);
        }

        fn date(&self) -> Date {
            Date::from_system_time(*self.0.lock().unwrap())
        }
    }

    /// Every record in the file, in file order — including the duplicates a
    /// last-wins reader would collapse.
    fn all_records(path: &Path) -> Vec<(Date, Counts)> {
        let text = std::fs::read_to_string(path).unwrap();
        text.lines().filter_map(parse_record).collect()
    }

    /// What a last-wins reader sees for `date`.
    fn last_for(path: &Path, date: Date) -> Option<Counts> {
        all_records(path)
            .into_iter()
            .rfind(|(d, _)| *d == date)
            .map(|(_, c)| c)
    }

    /// Day 20000 after the epoch (2024-10-04) and its neighbours: arbitrary,
    /// but far from any epoch edge and stable.
    const DAY: u64 = 20_000;

    #[test]
    fn a_day_that_rolls_over_while_the_process_runs_closes_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        let first = clock.date();
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();

        counter.mesh_request(false);
        counter.mesh_request(false);
        counter.web_request(true);

        // Midnight passes with the process running. The next observation is
        // what notices, and it must close the old day rather than add to it.
        clock.set_day(DAY + 1, 9);
        let second = clock.date();
        counter.web_request(true);
        counter.flush().unwrap();

        assert_ne!(first, second);
        assert_eq!(
            last_for(&path, first).map(|c| (c.mesh_requests, c.web_requests)),
            Some((2, 1)),
            "the day that ended must be written with exactly its own counts"
        );
        assert_eq!(
            last_for(&path, second).map(|c| (c.mesh_requests, c.web_requests)),
            Some((0, 1)),
            "the new day starts at zero, it does not inherit"
        );
        assert_eq!(counter.open_day().0, second);
    }

    #[test]
    fn an_idle_rollover_writes_no_empty_record() {
        // A blog nobody read yesterday must not get a row of zero-rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();

        clock.set_day(DAY + 3, 4);
        counter.flush().unwrap();
        counter.mesh_request(false);
        counter.flush().unwrap();

        let records = all_records(&path);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].0, clock.date());
    }

    #[test]
    fn a_restart_mid_day_continues_the_days_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        clock.set_day(DAY, 10);
        let today = clock.date();

        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        counter.mesh_request(false);
        counter.mesh_request(false);
        counter.mesh_request(false);
        counter.mesh_session();
        counter.flush().unwrap();
        drop(counter);

        // Same day, new process: the count resumes rather than restarting.
        clock.set_day(DAY, 16);
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        assert_eq!(
            counter.open_day(),
            (
                today,
                Counts {
                    mesh_requests: 3,
                    mesh_sessions: 1,
                    ..Counts::default()
                }
            ),
            "the open day must be resumed from the file, not started at zero"
        );
        counter.mesh_request(false);
        counter.flush().unwrap();

        assert_eq!(
            last_for(&path, today).map(|c| c.mesh_requests),
            Some(4),
            "the day's total must span the restart"
        );
    }

    #[test]
    fn a_clock_jump_backwards_does_not_corrupt_an_already_written_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        let earlier = clock.date();

        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        for _ in 0..500 {
            counter.web_request(true);
        }
        clock.set_day(DAY + 1, 2);
        let later = clock.date();
        counter.web_request(true);
        counter.flush().unwrap();
        assert_eq!(last_for(&path, earlier).map(|c| c.web_requests), Some(500));

        // NTP steps the clock back into the day that is already on disk.
        clock.set_day(DAY, 23);
        counter.web_request(true);
        counter.web_request(true);
        counter.flush().unwrap();

        assert_eq!(
            last_for(&path, earlier).map(|c| c.web_requests),
            Some(500),
            "the day already written must read the same as before the jump"
        );
        let after = last_for(&path, later).unwrap();
        assert_eq!(
            after.web_requests, 3,
            "the requests must land on the open day, not on the closed one"
        );
        assert_eq!(
            after.clock_behind, 2,
            "and the file must say the clock was behind when they did"
        );
        assert_eq!(counter.open_day().0, later, "the open day must not regress");
    }

    #[test]
    fn a_backwards_clock_across_a_restart_resumes_the_later_day() {
        // The dead-RTC-at-boot case: the file is ahead of the clock, and the
        // previous run's last day must not be reopened and rewritten smaller.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY + 5);
        let written = clock.date();

        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        for _ in 0..40 {
            counter.mesh_request(false);
        }
        counter.flush().unwrap();
        drop(counter);

        clock.set_day(DAY, 1);
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        assert_eq!(counter.open_day().0, written);
        assert_eq!(counter.open_day().1.mesh_requests, 40);
        counter.mesh_request(false);
        counter.flush().unwrap();

        assert_eq!(
            last_for(&path, written).map(|c| c.mesh_requests),
            Some(41),
            "the resumed day must grow, never shrink"
        );
        assert!(
            all_records(&path).iter().all(|(d, _)| *d == written),
            "no day earlier than the one in the file may be opened"
        );
    }

    #[test]
    fn a_kill_mid_write_loses_only_the_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        let first = clock.date();

        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        counter.mesh_request(false);
        counter.flush().unwrap();
        clock.set_day(DAY + 1, 5);
        counter.web_request(true);
        counter.flush().unwrap();
        drop(counter);

        // What an append killed halfway leaves behind: a prefix of a record,
        // with no newline after it.
        let torn = format!("{RECORD} date=2026-08-07 tz=UTC mesh_requests=99 web_re");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(torn.as_bytes()).unwrap();
        drop(file);

        clock.set_day(DAY + 1, 6);
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        assert_eq!(
            last_for(&path, first).map(|c| c.mesh_requests),
            Some(1),
            "the day before the kill must survive it intact"
        );
        assert_eq!(
            counter.open_day().1.web_requests,
            1,
            "and the completed record for the open day must still be resumed"
        );
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("web_re\n"),
            "the fragment must not be read as a record"
        );
    }

    #[test]
    fn a_key_this_version_does_not_know_survives_a_read_and_a_compaction() {
        // The extensibility rule, in both directions: an unknown key does not
        // stop the record from being read, and compacting copies the line
        // rather than re-serialising it, so the key is still there after.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        std::fs::write(
            &path,
            format!(
                "{HEADER}\
                 {RECORD} date=2024-01-01 tz=UTC mesh_requests=1 gopher_requests=7\n\
                 {RECORD} date=2024-01-01 tz=UTC mesh_requests=4 gopher_requests=9\n"
            ),
        )
        .unwrap();

        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        drop(counter);

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(all_records(&path).len(), 1, "duplicates must be collapsed");
        assert_eq!(
            last_for(&path, "2024-01-01".parse().unwrap()).map(|c| c.mesh_requests),
            Some(4),
            "the surviving record must be the last one, not the first"
        );
        assert!(
            text.contains("gopher_requests=9"),
            "compaction must not drop a field it does not understand: {text}"
        );
    }

    #[test]
    fn a_compaction_leaves_one_record_per_date() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_FILE_NAME);
        let clock = TestClock::at_day(DAY);
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        for hour in 0..6 {
            clock.set_day(DAY, hour);
            counter.mesh_request(false);
            counter.flush().unwrap();
        }
        assert_eq!(all_records(&path).len(), 6, "six flushes, six records");
        drop(counter);

        clock.set_day(DAY + 1, 0);
        let counter = Counter::open_with_clock(&path, clock.clock()).unwrap();
        drop(counter);
        let records = all_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1.mesh_requests, 6, "the total must be preserved");
    }

    #[test]
    fn a_disabled_counter_writes_nothing() {
        let counter = Counter::disabled();
        counter.mesh_request(true);
        counter.web_request(false);
        counter.flush().unwrap();
        assert_eq!(counter.open_day().1, Counts::default());
    }

    #[test]
    fn a_record_line_is_the_documented_shape() {
        // The bytes the README and the man page quote.
        let counts = Counts {
            mesh_requests: 41,
            mesh_sessions: 12,
            mesh_identified_requests: 0,
            web_requests: 308,
            web_not_found: 57,
            clock_behind: 0,
        };
        let written = UNIX_EPOCH + Duration::from_secs(20_672 * 86_400 + 23 * 3600 + 59 * 60 + 12);
        let line = format_record("2026-08-07".parse().unwrap(), &counts, written);
        assert_eq!(
            line,
            "DAY date=2026-08-07 tz=UTC mesh_requests=41 mesh_sessions=12 \
             mesh_identified_requests=0 web_requests=308 web_not_found=57 \
             clock_behind=0 written=2026-08-07T23:59:12Z\n"
        );
        assert_eq!(
            parse_record(&line),
            Some(("2026-08-07".parse().unwrap(), counts))
        );
    }
}
