//! `--watch`: a board's debug log, stamped and kept.
//!
//! A field walk leaves its only evidence on the debug CDC (if00). Every
//! walk so far read it with an ad hoc script that lived in /tmp and died
//! with the laptop — and took the only log of a field run with it
//! (Codeberg #365). This module is that script as a shipped tool: open
//! the port with DTR and RTS raised (the firmware transmits only with
//! both set, `Fd::set_debug_port`), stamp every line with wall-clock
//! time, append to a file that survives anything short of the disk, and
//! reconnect when the port vanishes — a reset, a reflash, an unplug —
//! logging the gap as its own line rather than exiting on EOF.
//!
//! Deliberately no filtering: the watch file is the evidence, and a
//! classifier that decided at capture time what matters would decide
//! wrongly exactly once. The view lives in [`crate::summarize`]. The one
//! exception is the torn first line after each (re)open
//! (`FirstLineSync`), which is discarded but accounted on its own
//! `[WATCH]` line — junk stamped as evidence is worse than a counted gap.
//!
//! Deliberately no daemon and no background mode: the person runs it in
//! a terminal, or under `nohup` themselves.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::flow;
use crate::manifest::Catalogue;
use crate::sys::Fd;
use crate::usb::{same_serial, Device, Sysfs};

/// `bInterfaceNumber` of the debug CDC — the ASCII text log. The
/// transport (if02) is [`crate::radio::TRANSPORT_INTERFACE`].
pub const DEBUG_INTERFACE: u8 = 0;

/// How long one read waits before looking again. Short enough that a
/// vanished port is noticed promptly; long enough not to spin.
const POLL: Duration = Duration::from_millis(200);

/// A board writing without newlines must not grow the line buffer
/// without bound; past this the buffer is flushed as a line of its own.
const CARRY_MAX: usize = 16 * 1024;

/// The `--watch` session: resolve which board (or path) to read, open
/// its debug port, and hand the loop a connector that reopens it for as
/// long as the process lives. Returns only on a setup error or a watch
/// file that stopped taking writes; a healthy watch ends with Ctrl-C.
pub fn watch(
    catalogue: &Catalogue,
    sysfs: &Sysfs,
    selector: &str,
    out: Option<&Path>,
    quiet: bool,
) -> io::Result<()> {
    if quiet && out.is_none() {
        return Err(io::Error::other(
            "--quiet without --out would watch into the void; give --out a file or drop --quiet",
        ));
    }
    let file = match out {
        Some(path) => Some(
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .map_err(|err| {
                    io::Error::new(err.kind(), format!("opening {}: {err}", path.display()))
                })?,
        ),
        None => None,
    };
    let mut sink = Sink {
        file,
        echo: !quiet,
        stamp: Box::new(stamp_now),
    };
    match choose_target(catalogue, sysfs, selector)? {
        // A path given directly: no bus identity to correlate, so a
        // reconnect is a reopen of the same path — which is also the
        // right thing for a by-id link, whose name follows the board.
        Target::Path(path) => {
            let mut first = Some(open_path(&path)?);
            let label = path.display().to_string();
            let mut connect = |attempt: u32| {
                if let Some(fd) = first.take() {
                    return Connect::Ready(fd, label.clone());
                }
                match open_path(&path) {
                    Ok(fd) => Connect::Ready(fd, label.clone()),
                    Err(_) => Connect::Wait(backoff(attempt)),
                }
            };
            watch_loop(&mut connect, &mut sink)
        }
        Target::Board(device) => {
            // The first open is a hard error — a user with no permission
            // on the port must learn it now, not retry into silence. The
            // reopens after a gap retry instead: EACCES right after a
            // re-enumeration is udev still applying its rules.
            let tty = crate::entry::wait_for_interface_tty(
                sysfs,
                &device,
                DEBUG_INTERFACE,
                Duration::from_secs(2),
            )?
            .ok_or_else(|| {
                io::Error::other(format!(
                    "{}: the debug port (if{DEBUG_INTERFACE:02}) never appeared",
                    device.name
                ))
            })?;
            let fd = flow::open_debug(sysfs, &device, &tty)?;
            let label = device.serial.clone().unwrap_or_else(|| device.name.clone());
            let mut first = Some((fd, tty));
            let mut connect = |attempt: u32| {
                if let Some((fd, tty)) = first.take() {
                    return Connect::Ready(fd, format!("{label} on {}", tty.display()));
                }
                let Some(tty) = debug_tty_now(sysfs, &device) else {
                    return Connect::Wait(backoff(attempt));
                };
                match flow::open_debug(sysfs, &device, &tty) {
                    Ok(fd) => Connect::Ready(fd, format!("{label} on {}", tty.display())),
                    Err(_) => Connect::Wait(backoff(attempt)),
                }
            };
            watch_loop(&mut connect, &mut sink)
        }
    }
}

/// What `--watch`'s optional value resolved to.
#[derive(Debug)]
pub(crate) enum Target {
    /// A value with a slash in it: a serial port path, opened as given.
    Path(PathBuf),
    /// A board found on the bus, followed across re-enumerations.
    Board(Device),
}

/// Resolve the `--watch` value the way the configure sessions find
/// boards: with no value and exactly one running board, that board; with
/// several, require the serial (or the bus port name, e.g. `3-2.4`).
pub(crate) fn choose_target(
    catalogue: &Catalogue,
    sysfs: &Sysfs,
    selector: &str,
) -> io::Result<Target> {
    if selector.contains('/') {
        return Ok(Target::Path(PathBuf::from(selector)));
    }
    let running: Vec<flow::Candidate> = flow::find_candidates(catalogue, sysfs)
        .map_err(io::Error::other)?
        .into_iter()
        .filter(|c| !c.in_bootloader)
        .collect();
    let listed = || {
        running
            .iter()
            .map(|c| c.describe())
            .collect::<Vec<_>>()
            .join("\n")
    };
    if selector.is_empty() {
        return match running.len() {
            0 => Err(io::Error::other(
                "no running LNode on the bus. --watch reads a running board's debug log; a \
                 board in its bootloader has none.",
            )),
            1 => Ok(Target::Board(
                running
                    .into_iter()
                    .next()
                    .map(|c| c.device)
                    .ok_or_else(|| {
                        io::Error::other("the one candidate vanished between count and take")
                    })?,
            )),
            _ => Err(io::Error::other(format!(
                "several running boards; name one with --watch <serial>:\n{}",
                listed()
            ))),
        };
    }
    let matches = |device: &Device| {
        device.name == selector
            || device
                .serial
                .as_deref()
                .is_some_and(|serial| same_serial(serial, selector))
    };
    match running.iter().position(|c| matches(&c.device)) {
        Some(index) => {
            let mut running = running;
            Ok(Target::Board(running.swap_remove(index).device))
        }
        None => Err(io::Error::other(format!(
            "no running board answers to {selector:?}; attached and running are:\n{}",
            listed()
        ))),
    }
}

/// Where this board's debug tty is right now, or `None` while it is off
/// the bus (rebooting, reflashing, or sitting in its bootloader — which
/// [`Device::is_same_board`] still recognises but which has no if00).
fn debug_tty_now(sysfs: &Sysfs, device: &Device) -> Option<PathBuf> {
    sysfs
        .devices()
        .ok()?
        .into_iter()
        .find(|d| d.is_same_board(device))
        .and_then(|d| d.interface(DEBUG_INTERFACE)?.tty.clone())
        .map(|tty| sysfs.stable_tty_path(&tty))
}

fn open_path(path: &Path) -> io::Result<Fd> {
    let fd = Fd::open_serial(path)?;
    fd.set_debug_port()?;
    Ok(fd)
}

/// What the connector decided for one attempt.
pub(crate) enum Connect {
    /// An open port to read, and what to call it in the log lines.
    Ready(Fd, String),
    /// Nothing to open yet; wait this long and ask again.
    Wait(Duration),
    /// End the watch. Production connectors never say this — the loop
    /// runs until the process is killed — but the tests need an exit,
    /// which is also why the non-test build sees it as unconstructed.
    #[allow(dead_code)]
    Stop,
}

/// 250 ms doubling to a 5 s ceiling: quick enough that the first lines
/// after a reset are caught, bounded so a board that stays away does not
/// busy-poll the bus.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let ms = 250u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(ms.min(5_000))
}

/// The loop: connect, pump until the port goes away, log the gap,
/// reconnect. Only the sink can fail it — the watch file refusing a
/// write means the evidence is no longer being kept, which is the one
/// thing this tool must not be silent about.
pub(crate) fn watch_loop<W: Write>(
    connect: &mut dyn FnMut(u32) -> Connect,
    sink: &mut Sink<W>,
) -> io::Result<()> {
    let mut attempt = 0u32;
    let mut gap_started: Option<Instant> = None;
    loop {
        match connect(attempt) {
            Connect::Stop => return Ok(()),
            Connect::Wait(delay) => {
                std::thread::sleep(delay);
                attempt = attempt.saturating_add(1);
            }
            Connect::Ready(fd, name) => {
                match gap_started.take() {
                    Some(since) => sink.line(&reconnect_line(&name, since.elapsed()))?,
                    None => sink.line(&start_line(&name))?,
                }
                attempt = 0;
                let reason = pump(&fd, sink)?;
                sink.line(&gap_line(&name, &reason))?;
                gap_started = Some(Instant::now());
            }
        }
    }
}

fn start_line(name: &str) -> String {
    format!("[WATCH] watching {name}")
}

/// The gap's own line, written the moment the port goes away — so a
/// watch file read later shows where the log has a hole and why, instead
/// of a silent jump in the timestamps.
pub(crate) fn gap_line(name: &str, reason: &str) -> String {
    format!("[WATCH] {name} went away ({reason}); reconnecting")
}

pub(crate) fn reconnect_line(name: &str, gap: Duration) -> String {
    format!(
        "[WATCH] reconnected to {name} after {:.1}s gap",
        gap.as_secs_f64()
    )
}

/// Read stamped lines until the port goes away; the returned string is
/// the reason it ended. Only a sink failure is a hard error.
fn pump<W: Write>(fd: &Fd, sink: &mut Sink<W>) -> io::Result<String> {
    let mut carry: Vec<u8> = Vec::new();
    let mut sync = FirstLineSync::new();
    loop {
        if let Some(reason) = drain_once(fd, sink, &mut carry, &mut sync)? {
            return Ok(reason);
        }
    }
}

/// The first line after (re)open may be torn: the port's buffer can hold
/// bytes written before DTR was raised, so the first read opens mid-line
/// — the bench watch stamped `[STACK] … region=0x20004c40..0x20020[GNSS]
/// bytes=0 …`, two board lines glued at the tear. Until the first
/// newline the stream is not at a line boundary; those bytes are
/// discarded and accounted with one `[WATCH] discarded partial first
/// line (<n> bytes)` line instead of being stamped as evidence.
pub(crate) struct FirstLineSync {
    /// `Some(n)`: still before the first newline, `n` bytes discarded so
    /// far. `None`: synced — every further byte belongs to real lines.
    discarded: Option<usize>,
}

impl FirstLineSync {
    pub(crate) fn new() -> Self {
        Self { discarded: Some(0) }
    }

    /// Consume the pre-newline bytes from `carry`, leaving it at a line
    /// boundary once the first newline arrives. An open that lands
    /// exactly on a boundary (the first byte is the newline) lost
    /// nothing and writes no marker.
    fn apply<W: Write>(&mut self, sink: &mut Sink<W>, carry: &mut Vec<u8>) -> io::Result<()> {
        let Some(count) = self.discarded.as_mut() else {
            return Ok(());
        };
        match carry.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                let total = *count + pos;
                carry.drain(..=pos);
                self.discarded = None;
                if total > 0 {
                    sink.line(&discard_line(total))?;
                }
            }
            None => {
                *count += carry.len();
                carry.clear();
            }
        }
        Ok(())
    }

    /// The port went away before any newline: everything it said was the
    /// torn first line. Report the count rather than stamping the junk.
    fn finish<W: Write>(&mut self, sink: &mut Sink<W>, carry: &mut Vec<u8>) -> io::Result<()> {
        let Some(count) = self.discarded.take() else {
            return Ok(());
        };
        let total = count + carry.len();
        carry.clear();
        if total > 0 {
            sink.line(&discard_line(total))?;
        }
        Ok(())
    }
}

fn discard_line(n: usize) -> String {
    format!("[WATCH] discarded partial first line ({n} bytes)")
}

/// One read step of [`pump`], split out so a test can interleave reads
/// with the device side deterministically. `Some(reason)` when the port
/// went away; whatever half-line was buffered is flushed first, because
/// a board resetting mid-line still said it.
pub(crate) fn drain_once<W: Write>(
    fd: &Fd,
    sink: &mut Sink<W>,
    carry: &mut Vec<u8>,
    sync: &mut FirstLineSync,
) -> io::Result<Option<String>> {
    match fd.read_available(POLL) {
        Ok(Some(bytes)) => {
            carry.extend_from_slice(&bytes);
            sync.apply(sink, carry)?;
            while let Some(pos) = carry.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = carry.drain(..=pos).collect();
                sink.line(&decode(&line))?;
            }
            if carry.len() > CARRY_MAX {
                let flushed: Vec<u8> = std::mem::take(carry);
                sink.line(&decode(&flushed))?;
            }
            Ok(None)
        }
        Ok(None) => {
            sync.finish(sink, carry)?;
            flush_carry(sink, carry)?;
            Ok(Some("EOF, the port closed".into()))
        }
        Err(err) => {
            sync.finish(sink, carry)?;
            flush_carry(sink, carry)?;
            Ok(Some(format!("read failed: {err}")))
        }
    }
}

fn flush_carry<W: Write>(sink: &mut Sink<W>, carry: &mut Vec<u8>) -> io::Result<()> {
    if carry.is_empty() {
        return Ok(());
    }
    let flushed: Vec<u8> = std::mem::take(carry);
    sink.line(&decode(&flushed))
}

fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

/// Where the stamped lines go: the watch file (append, flushed per line,
/// so a crash or a dying battery loses at most the line in flight) and,
/// as a courtesy, stdout. Stdout going away must not end the watch — the
/// file is the evidence — so only the file's errors count.
pub(crate) struct Sink<W: Write> {
    pub(crate) file: Option<W>,
    pub(crate) echo: bool,
    pub(crate) stamp: Box<dyn FnMut() -> String>,
}

impl<W: Write> Sink<W> {
    pub(crate) fn line(&mut self, raw: &str) -> io::Result<()> {
        let stamped = format!("{} {raw}\n", (self.stamp)());
        if let Some(file) = &mut self.file {
            file.write_all(stamped.as_bytes())?;
            file.flush()?;
        }
        if self.echo {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(stamped.as_bytes());
            let _ = stdout.flush();
        }
        Ok(())
    }
}

/// Now, as an ISO-8601 local-time stamp with milliseconds.
fn stamp_now() -> String {
    let millis = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_millis() as i64,
        // A host clock before 1970 still gets a correct (negative) stamp
        // rather than a panic in the middle of a field walk.
        Err(err) => -(err.duration().as_millis() as i64),
    };
    iso8601(millis, crate::sys::utc_offset_secs(millis.div_euclid(1000)))
}

/// Render Unix milliseconds at a UTC offset as ISO-8601 with
/// milliseconds: `2026-09-04T15:22:27.123+02:00`. Pure — the two
/// syscalls that feed it live in [`crate::sys`] — so the format the
/// watch files carry is pinned by tests, not by the host's C library.
pub(crate) fn iso8601(unix_millis: i64, utc_offset_secs: i64) -> String {
    let millis = unix_millis.rem_euclid(1000);
    let local_secs = unix_millis.div_euclid(1000) + utc_offset_secs;
    let days = local_secs.div_euclid(86_400);
    let time_of_day = local_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let sign = if utc_offset_secs < 0 { '-' } else { '+' };
    let offset = utc_offset_secs.abs();
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}{sign}{:02}:{:02}",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
        offset / 3600,
        (offset % 3600) / 60,
    )
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic
/// Gregorian calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year + i64::from(month <= 2), month as u32, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::testpty::Pty;
    use crate::usb::UsbId;
    use std::sync::mpsc;

    // -----------------------------------------------------------------
    // The timestamp format
    // -----------------------------------------------------------------

    #[test]
    fn the_stamp_is_iso8601_local_time_with_milliseconds() {
        // The #365 walk's own moment: 15:22:27 CEST is +02:00.
        assert_eq!(
            iso8601(1_788_528_147_123, 7_200),
            "2026-09-04T15:22:27.123+02:00"
        );
        // UTC renders as +00:00, not as Z: one shape for every stamp.
        assert_eq!(iso8601(0, 0), "1970-01-01T00:00:00.000+00:00");
    }

    #[test]
    fn a_negative_and_a_half_hour_offset_render_correctly() {
        // Newfoundland's -03:30: sign on the offset, minutes kept.
        assert_eq!(
            iso8601(1_767_600_900_000, -12_600),
            "2026-01-05T04:45:00.000-03:30"
        );
    }

    #[test]
    fn the_calendar_arithmetic_knows_a_leap_day() {
        assert_eq!(iso8601(951_868_799_000, 0), "2000-02-29T23:59:59.000+00:00");
    }

    // -----------------------------------------------------------------
    // The gap and reconnect lines
    // -----------------------------------------------------------------

    #[test]
    fn the_gap_is_its_own_line_naming_the_port_and_the_reason() {
        assert_eq!(
            gap_line("183004F712B4A7FE on /dev/ttyACM1", "EOF, the port closed"),
            "[WATCH] 183004F712B4A7FE on /dev/ttyACM1 went away (EOF, the port closed); \
             reconnecting"
        );
        assert_eq!(
            reconnect_line(
                "183004F712B4A7FE on /dev/ttyACM1",
                Duration::from_millis(12_400)
            ),
            "[WATCH] reconnected to 183004F712B4A7FE on /dev/ttyACM1 after 12.4s gap"
        );
    }

    #[test]
    fn the_backoff_doubles_and_stays_bounded() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_millis(1_000));
        assert_eq!(backoff(5), Duration::from_secs(5));
        // Bounded means bounded: no overflow, no runaway wait.
        assert_eq!(backoff(u32::MAX), Duration::from_secs(5));
    }

    // -----------------------------------------------------------------
    // Reading a port (a pty plays the board)
    // -----------------------------------------------------------------

    /// A sink writing into a Vec, stamping with a counter so the output
    /// is deterministic.
    fn test_sink() -> Sink<Vec<u8>> {
        let mut n = 0u32;
        Sink {
            file: Some(Vec::new()),
            echo: false,
            stamp: Box::new(move || {
                n += 1;
                format!("t{n}")
            }),
        }
    }

    fn sunk(sink: Sink<Vec<u8>>) -> String {
        String::from_utf8(sink.file.unwrap_or_default()).expect("sink output is UTF-8")
    }

    #[test]
    fn lines_are_stamped_and_a_partial_line_survives_the_disconnect() {
        let pty = Pty::open();
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        fd.set_debug_port().unwrap();
        let mut sink = test_sink();
        let mut carry = Vec::new();
        let mut sync = FirstLineSync::new();

        // The pty discards unread bytes when its master closes, so the
        // reads are interleaved with the writes rather than raced. The
        // first line (29 bytes before its newline, the CR counted) is the
        // possibly-torn one and becomes the discard marker.
        pty.write_raw(b"[LORA] RX 183 bytes rssi=-69\r\n[BOOT] hello\npartial");
        assert_eq!(
            drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap(),
            None
        );
        drop(pty);
        let reason = drain_once(&fd, &mut sink, &mut carry, &mut sync)
            .unwrap()
            .expect("master closed, the port is gone");
        assert!(reason.contains("EOF"), "{reason}");
        assert_eq!(
            sunk(sink),
            "t1 [WATCH] discarded partial first line (29 bytes)\n\
             t2 [BOOT] hello\nt3 partial\n",
            "first line discarded and marked, every line stamped, the \
             half-line flushed on the gap"
        );
    }

    #[test]
    fn the_torn_first_line_after_open_is_discarded_and_marked() {
        // The bench observation: the port's buffer held a partial line
        // from before DTR, so the first read opens mid-line — two board
        // lines glued at the tear
        // (`[STACK] … region=0x20004c40..0x20020[GNSS] bytes=0 …`).
        let pty = Pty::open();
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        fd.set_debug_port().unwrap();
        let mut sink = test_sink();
        let mut carry = Vec::new();
        let mut sync = FirstLineSync::new();

        pty.write_raw(b"0x20020[GNSS] bytes=0 fixes=0\n[STACK] free=1234\n");
        assert_eq!(
            drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap(),
            None
        );
        drop(pty);
        let _ = drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap();
        assert_eq!(
            sunk(sink),
            "t1 [WATCH] discarded partial first line (29 bytes)\n\
             t2 [STACK] free=1234\n",
            "the torn tail is counted, the first complete line survives"
        );
    }

    #[test]
    fn the_discard_counts_across_reads_and_an_exact_boundary_costs_nothing() {
        // A torn tail arriving in two reads is still one count; a port
        // whose first byte is the newline lost nothing and gets no marker.
        let pty = Pty::open();
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        fd.set_debug_port().unwrap();
        let mut sink = test_sink();
        let mut carry = Vec::new();
        let mut sync = FirstLineSync::new();

        pty.write_raw(b"torn");
        assert_eq!(
            drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap(),
            None
        );
        pty.write_raw(b"-tail\nhello\n");
        assert_eq!(
            drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap(),
            None
        );
        assert_eq!(
            sunk(sink),
            "t1 [WATCH] discarded partial first line (9 bytes)\nt2 hello\n"
        );

        let pty = Pty::open();
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        fd.set_debug_port().unwrap();
        let mut sink = test_sink();
        let mut carry = Vec::new();
        let mut sync = FirstLineSync::new();
        pty.write_raw(b"\nhello\n");
        assert_eq!(
            drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap(),
            None
        );
        assert_eq!(sunk(sink), "t1 hello\n", "no marker for a lossless open");
    }

    #[test]
    fn a_port_that_dies_before_any_newline_reports_the_discard_not_the_junk() {
        let pty = Pty::open();
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        fd.set_debug_port().unwrap();
        let mut sink = test_sink();
        let mut carry = Vec::new();
        let mut sync = FirstLineSync::new();

        pty.write_raw(b"torn");
        assert_eq!(
            drain_once(&fd, &mut sink, &mut carry, &mut sync).unwrap(),
            None
        );
        drop(pty);
        let reason = drain_once(&fd, &mut sink, &mut carry, &mut sync)
            .unwrap()
            .expect("master closed, the port is gone");
        assert!(reason.contains("EOF"), "{reason}");
        assert_eq!(
            sunk(sink),
            "t1 [WATCH] discarded partial first line (4 bytes)\n",
            "everything before the first newline is the torn line, junk \
             is never stamped as evidence"
        );
    }

    #[test]
    fn the_loop_logs_the_gap_and_reads_on_across_a_reconnect() {
        // Two ptys play the board before and after a reset. The closer
        // thread drops each pty only after the sink has echoed what was
        // written to it — a pty discards unread bytes at master close,
        // so closing on a timer would race the reader (zero-flake rule).
        let (tx, rx) = mpsc::channel::<String>();
        let pty1 = Pty::open();
        let pty2 = Pty::open();
        let slave1 = pty1.slave_path.clone();
        let slave2 = pty2.slave_path.clone();
        // Each session opens onto a torn tail (discarded and marked) and
        // then a real line, exactly like a board whose buffer held a
        // partial line from before DTR.
        pty1.write_raw(b"tail1\nalpha\n");
        pty2.write_raw(b"tail2\nbeta\n");
        let closer = std::thread::spawn(move || {
            let mut pty1 = Some(pty1);
            let mut pty2 = Some(pty2);
            while let Ok(line) = rx.recv() {
                if line.contains("alpha") {
                    drop(pty1.take());
                }
                if line.contains("beta") {
                    drop(pty2.take());
                    break;
                }
            }
        });

        struct Tee(Vec<u8>, mpsc::Sender<String>);
        impl Write for Tee {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.extend_from_slice(buf);
                let _ = self.1.send(String::from_utf8_lossy(buf).to_string());
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut n = 0u32;
        let mut sink = Sink {
            file: Some(Tee(Vec::new(), tx)),
            echo: false,
            stamp: Box::new(move || {
                n += 1;
                format!("t{n}")
            }),
        };

        let mut stage = 0u32;
        let mut connect = |_attempt: u32| {
            stage += 1;
            let slave = match stage {
                1 => &slave1,
                2 => &slave2,
                _ => return Connect::Stop,
            };
            let fd = Fd::open_serial(slave).expect("opening the pty slave");
            fd.set_debug_port().expect("a pty tolerates the ioctl");
            Connect::Ready(fd, format!("board (session {stage})"))
        };
        watch_loop(&mut connect, &mut sink).unwrap();
        closer.join().expect("the closer thread ends with the ptys");

        let said = String::from_utf8(sink.file.map(|t| t.0).unwrap_or_default()).unwrap();
        let expected = [
            "t1 [WATCH] watching board (session 1)",
            "t2 [WATCH] discarded partial first line (5 bytes)",
            "t3 alpha",
            "t4 [WATCH] board (session 1) went away (EOF, the port closed); reconnecting",
            "t5 [WATCH] reconnected to board (session 2) after ",
            "t6 [WATCH] discarded partial first line (5 bytes)",
            "t7 beta",
            "t8 [WATCH] board (session 2) went away (EOF, the port closed); reconnecting",
        ];
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines.len(), expected.len(), "{said}");
        for (line, want) in lines.iter().zip(expected) {
            assert!(line.starts_with(want), "wanted {want:?}, got {line:?}");
        }
        assert!(lines[4].ends_with("s gap"), "{said}");
    }

    // -----------------------------------------------------------------
    // Choosing the board
    // -----------------------------------------------------------------

    fn fixture() -> (Catalogue, Sysfs) {
        (
            Catalogue::builtin().unwrap(),
            Sysfs::new(crate::sysfs_fixture::materialized()),
        )
    }

    #[test]
    fn a_value_with_a_slash_is_a_port_path_and_touches_no_bus() {
        // Even a bus-less host can watch a port it names directly.
        let catalogue = Catalogue::builtin().unwrap();
        let sysfs = Sysfs::new("/nonexistent/sysfs/root");
        match choose_target(&catalogue, &sysfs, "/dev/ttyACM7").unwrap() {
            Target::Path(path) => assert_eq!(path, PathBuf::from("/dev/ttyACM7")),
            Target::Board(_) => panic!("a path must not become a board"),
        }
    }

    #[test]
    fn with_several_boards_the_bare_flag_refuses_and_lists_them() {
        // The fixture bus has two running boards; guessing between them
        // would watch the wrong one silently.
        let (catalogue, sysfs) = fixture();
        let err = choose_target(&catalogue, &sysfs, "").unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("several running boards"), "{text}");
        assert!(text.contains("183004F712B4A7FE"), "{text}");
        assert!(text.contains("DEC9947DAD9D2869"), "{text}");
    }

    #[test]
    fn a_serial_names_the_board_even_in_its_bootloader_spelling() {
        let (catalogue, sysfs) = fixture();
        for spelling in ["183004F712B4A7FE", "183004f712b4a7fe", "12B4A7FE183004F7"] {
            match choose_target(&catalogue, &sysfs, spelling).unwrap() {
                Target::Board(device) => assert_eq!(device.name, "3-2.3.1", "{spelling}"),
                Target::Path(_) => panic!("{spelling} is a serial, not a path"),
            }
        }
    }

    #[test]
    fn the_bus_port_name_works_where_a_serial_is_unknown() {
        let (catalogue, sysfs) = fixture();
        match choose_target(&catalogue, &sysfs, "3-2.3.4.4").unwrap() {
            Target::Board(device) => {
                assert_eq!(device.id, UsbId::new(0x1209, 0x0002));
            }
            Target::Path(_) => panic!("a bus port name is not a path"),
        }
    }

    #[test]
    fn an_unknown_selector_is_refused_with_what_is_actually_there() {
        let (catalogue, sysfs) = fixture();
        let err = choose_target(&catalogue, &sysfs, "CAFEBABE00000000").unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("CAFEBABE00000000"), "{text}");
        assert!(text.contains("183004F712B4A7FE"), "{text}");
    }

    #[test]
    fn a_board_in_its_bootloader_is_not_offered_for_watching() {
        // 3-2.4 is the fixture's bootloader; it has no debug CDC.
        let (catalogue, sysfs) = fixture();
        let err = choose_target(&catalogue, &sysfs, "3-2.4").unwrap_err();
        assert!(format!("{err}").contains("no running board"), "{err}");
    }
}
