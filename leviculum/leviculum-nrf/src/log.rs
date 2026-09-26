//! Debug logging via USB CDC-ACM
//!
//! All log output goes into a static ring buffer (`LOG_RING`). The USB debug
//! writer task drains the ring buffer to the host when DTR is asserted.
//!
//! This design never blocks, never drops messages for the reader, and works
//! regardless of when the host opens the port. Old data is overwritten when
//! the ring buffer is full (like Linux `dmesg` / `printk`).
//!
//! Every line carries a board-side `t=<uptime_ms>` stamp, appended at the
//! end. The host cannot supply one: the ring is drained in 64-byte USB
//! packets on a 100 ms loop, so a capture's arrival times measure the
//! drain, not the board. Shape and rationale in `leviculum-log-line`.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

// Ring buffer — 8 KiB. Boot-critical output is ~1-2 KiB; runtime
// output is gated on RUNTIME_DRAIN_OPEN so it doesn't lap the
// scrollback before the host attaches. Started at 32 KiB during the
// Stage 1B refactor but BSS-pressure forced it down: stack_free
// dropped to 36 bytes at 96-KiB-heap+32-KiB-ring, ~16 KiB stack
// headroom now (with HEAP_SIZE=64 KiB and the 8 KiB ring) — measured.
const LOG_RING_SIZE: usize = 8 * 1024;
// Persistent log tail kept in `.uninit` RAM that survives `sys_reset`.
// 2 KiB ring written in parallel to the main LOG_RING by every
// `log_fmt*` call. Read once at boot-startup so a board that crashed
// can replay the last ~2 KiB of its log before the crash. Best-effort
// — a HardFault that hits between `LOG_RING.write` and the parallel
// `PERSISTENT_TAIL.write` loses that line.
const PERSISTENT_TAIL_SIZE: usize = 2048;
const PERSISTENT_TAIL_MAGIC: u32 = 0x10A9_11A1;

/// True once the host has DTR-asserted on the debug CDC port (or
/// after the 30 s headless-fallback timeout). Producer-side runtime
/// log calls (`log_fmt`, `info!`, `warn!`) silently drop their output
/// until this flips, so the LOG_RING isn't lapped by LoRa traffic
/// before the host gets a chance to read the boot-critical lines.
/// Boot-critical paths use `log_fmt_critical` / `log_critical!` and
/// bypass this gate. Set by `usb::debug_writer_task` on first
/// DTR-assert observation, or by `runtime_drain_open_timeout_task`
/// after 30 s.
pub static RUNTIME_DRAIN_OPEN: AtomicBool = AtomicBool::new(false);
/// Counter for runtime log lines dropped because RUNTIME_DRAIN_OPEN
/// was still false. Logged once at gate-open so the field operator
/// knows how many lines were lost while waiting.
pub static RUNTIME_DROPPED: AtomicUsize = AtomicUsize::new(0);

/// Lock-free ring buffer for log output.
/// Single producer (log_fmt / tracing subscriber), single consumer (USB debug writer).
pub struct LogRing {
    buf: core::cell::UnsafeCell<[u8; LOG_RING_SIZE]>,
    write_pos: AtomicUsize,
    read_pos: AtomicUsize,
}

// Safety: Embassy is single-threaded (cooperative executor on one core).
// One producer (log_fmt, called from any task), one consumer (debug_writer_task).
unsafe impl Sync for LogRing {}

impl Default for LogRing {
    fn default() -> Self {
        Self::new()
    }
}

impl LogRing {
    pub const fn new() -> Self {
        Self {
            buf: core::cell::UnsafeCell::new([0u8; LOG_RING_SIZE]),
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
        }
    }

    /// Write bytes into the ring buffer. Overwrites oldest data if full.
    ///
    /// Sound in both the normal (task) and fault/exception path: never
    /// forms a `&mut` to the shared buffer, and reserves the byte range
    /// atomically so a log fired from an interrupt while a task is
    /// mid-write cannot alias the buffer or race the position update.
    /// Each producer owns a disjoint reserved range and writes it with
    /// raw volatile stores. No critical section, so it is safe to call
    /// from the SoftDevice-masked fault path.
    pub fn write(&self, data: &[u8]) {
        let buf = self.buf.get().cast::<u8>();
        // Reserve `data.len()` bytes atomically. `start` is this
        // producer's exclusive range; concurrent producers get
        // non-overlapping ranges.
        let start = self.write_pos.fetch_add(data.len(), Ordering::AcqRel);
        for (i, &byte) in data.iter().enumerate() {
            let idx = start.wrapping_add(i) % LOG_RING_SIZE;
            unsafe { core::ptr::write_volatile(buf.add(idx), byte) };
        }
        let wp = start.wrapping_add(data.len());

        // If writer has lapped reader, advance reader to oldest available data
        let rp = self.read_pos.load(Ordering::Relaxed);
        if wp.wrapping_sub(rp) > LOG_RING_SIZE {
            self.read_pos
                .store(wp.wrapping_sub(LOG_RING_SIZE), Ordering::Release);
        }
    }

    /// Read up to `out.len()` bytes. Returns number of bytes read.
    ///
    /// Uses raw volatile reads rather than forming a `&` to the shared
    /// buffer, so it never aliases a concurrent producer's raw writes.
    pub fn read(&self, out: &mut [u8]) -> usize {
        let buf = self.buf.get().cast::<u8>();
        let wp = self.write_pos.load(Ordering::Acquire);
        let mut rp = self.read_pos.load(Ordering::Relaxed);
        let avail = wp.wrapping_sub(rp);
        let to_read = avail.min(out.len());
        for slot in out.iter_mut().take(to_read) {
            *slot = unsafe { core::ptr::read_volatile(buf.add(rp % LOG_RING_SIZE)) };
            rp = rp.wrapping_add(1);
        }
        self.read_pos.store(rp, Ordering::Release);
        to_read
    }
}

/// Global ring buffer for all log output.
pub static LOG_RING: LogRing = LogRing::new();

/// Signal to wake the USB debug writer when new data is available.
pub static LOG_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// Log formatting
/// Milliseconds of board uptime, for the `t=` stamp every log line carries.
///
/// # Why this is safe from every context that logs
///
/// `Instant::now()` on this target resolves to embassy-nrf's RTC1 driver:
/// a relaxed load of the driver's `period` counter and a read of the RTC1
/// `COUNTER` register (`embassy-nrf-0.9.0/src/time_driver.rs:344-350`).
/// No critical section, no lock, no allocation, nothing that can panic —
/// so it is as safe in `log_fmt_critical`'s boot-phase and PMRT-replay
/// callers as in a task.
///
/// It is *meaningful* from every logging context too, because both
/// firmware entry points call `embassy_nrf::init` — which starts RTC1 —
/// as the FIRST statement of `main`, before any log call
/// (`bin/t114.rs:42`, `bin/rak4631.rs:43`). The panic handler
/// (`lib.rs:714`) and the `HardFault` exception (`lib.rs:954`) never log:
/// both write their evidence to `.uninit` RAM and `sys_reset`, and it is
/// the NEXT boot that logs it — with its own running clock. There is
/// therefore no reachable call site where the driver is un-started and no
/// line that needs a sentinel. `lnode_debug_log_format.rs` pins that
/// ordering so it stays true.
fn uptime_ms() -> u64 {
    embassy_time::Instant::now().as_millis()
}

/// Format a log message into a caller-provided stack buffer: the given
/// prefix, the body, the ` t=<uptime_ms>` stamp, and a trailing CRLF.
/// Returns the slice of valid bytes.
fn fmt_into<'a>(buf: &'a mut [u8; 1024], prefix: &str, args: core::fmt::Arguments) -> &'a [u8] {
    leviculum_log_line::format_line(buf.as_mut_slice(), prefix, args, uptime_ms())
}

/// Format and write a log message to the ring buffer. Never blocks.
///
/// **Runtime-gated:** silently drops if `RUNTIME_DRAIN_OPEN == false`.
/// Use `log_fmt_critical` for boot/panic/init-phase output that must
/// reach the host even before DTR-assert.
pub fn log_fmt(prefix: &str, args: core::fmt::Arguments) {
    if !RUNTIME_DRAIN_OPEN.load(Ordering::Relaxed) {
        RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut buf = [0u8; 1024];
    let bytes = fmt_into(&mut buf, prefix, args);
    LOG_RING.write(bytes);
    PERSISTENT_TAIL.write(bytes);
    LOG_SIGNAL.signal(());
}

/// Format and write a log message bypassing the runtime-drain gate.
/// Used for boot-phase logs, panic-PMRT replay, NVIC priority dump,
/// `[BUG32_RAM]` capture, and anything that must survive a slow-
/// attaching host or a board that crashes before DTR-assert.
pub fn log_fmt_critical(prefix: &str, args: core::fmt::Arguments) {
    let mut buf = [0u8; 1024];
    let bytes = fmt_into(&mut buf, prefix, args);
    LOG_RING.write(bytes);
    PERSISTENT_TAIL.write(bytes);
    LOG_SIGNAL.signal(());
}

/// Display adapter that escapes CR/LF as `\r`/`\n` literals so a
/// multi-line payload stays ONE log line. The SoftDevice fault panic
/// message embeds a raw `\n` after the source location; replaying it
/// verbatim split the `[PANIC_PMRT]` line mid-message and every
/// line-oriented capture reader lost the PC/PREGION tail.
pub struct EscapeCtrl<'a>(pub &'a str);

impl core::fmt::Display for EscapeCtrl<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use core::fmt::Write;
        for c in self.0.chars() {
            match c {
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                c => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

// Macros
/// Log an informational message over USB CDC-ACM debug port.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::log_fmt("[INFO] ", core::format_args!($($arg)*))
    };
}

/// Log a warning message over USB CDC-ACM debug port.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::log_fmt("[WARN] ", core::format_args!($($arg)*))
    };
}

/// Log a boot/panic-critical message that bypasses the
/// `RUNTIME_DRAIN_OPEN` gate. Use only for output that must reach
/// the host before DTR-assert (boot banners, NVIC dump, PMRT replay,
/// `[BUG32_RAM]`-style probes); routine info!/warn!/tracing-debug
/// stays gated.
#[macro_export]
macro_rules! log_critical {
    ($($arg:tt)*) => {
        $crate::log::log_fmt_critical("[INFO!] ", core::format_args!($($arg)*))
    };
}

// Persistent log tail in `.retained` (memory.x) — survives sys_reset.
// Its former `.uninit` home sat in the top 2 KiB of RAM, inside the
// Adafruit bootloader's stack; how much of the tail survived a reset
// depended on how deep that stack got (see `boot_trace::BOOT_TRACE`).
#[repr(C)]
pub struct PersistentTail {
    pub magic: u32,
    // Atomic so the byte-range reservation is race-free between a task
    // and the fault/exception path (same treatment as `LogRing`). Same
    // size/layout as a plain `u32`, so it survives `sys_reset` in the
    // `.retained` region exactly as before.
    pub write_pos: AtomicU32,
    pub buf: [u8; PERSISTENT_TAIL_SIZE],
}

#[link_section = ".retained"]
static mut PERSISTENT_TAIL_RAW: core::mem::MaybeUninit<PersistentTail> =
    core::mem::MaybeUninit::uninit();

/// Single-producer mirror ring written in parallel to LOG_RING by every
/// `log_fmt*` call. Read once at boot via `take_persistent_log_lines`.
pub struct PersistentTailMirror;
pub static PERSISTENT_TAIL: PersistentTailMirror = PersistentTailMirror;

impl PersistentTailMirror {
    pub fn write(&self, data: &[u8]) {
        unsafe {
            let p = core::ptr::addr_of_mut!(PERSISTENT_TAIL_RAW).cast::<PersistentTail>();
            let wp_atomic: &AtomicU32 = &*core::ptr::addr_of!((*p).write_pos);
            // Initialize on first write per boot if magic isn't ours yet.
            // (After take_persistent_log clears the magic, we
            // re-initialize — that's intentional: the tail captures the
            // *current* boot's tail, not the previous boot's.)
            let magic = core::ptr::read_volatile(core::ptr::addr_of!((*p).magic));
            if magic != PERSISTENT_TAIL_MAGIC {
                wp_atomic.store(0, Ordering::Release);
                let buf_ptr = core::ptr::addr_of_mut!((*p).buf).cast::<u8>();
                for i in 0..PERSISTENT_TAIL_SIZE {
                    core::ptr::write_volatile(buf_ptr.add(i), 0);
                }
                core::ptr::write_volatile(
                    core::ptr::addr_of_mut!((*p).magic),
                    PERSISTENT_TAIL_MAGIC,
                );
            }
            // Reserve the byte range atomically (no `&mut`, no non-atomic
            // RMW), then fill it with raw volatile stores.
            let start = wp_atomic.fetch_add(data.len() as u32, Ordering::AcqRel) as usize;
            let buf_ptr = core::ptr::addr_of_mut!((*p).buf).cast::<u8>();
            for (i, &b) in data.iter().enumerate() {
                core::ptr::write_volatile(
                    buf_ptr.add(start.wrapping_add(i) % PERSISTENT_TAIL_SIZE),
                    b,
                );
            }
        }
    }
}

/// Snapshot the persistent log tail from the previous boot, if any,
/// then clear the magic so the next call returns None until the
/// current boot starts writing again.
pub struct PersistentLogSnapshot {
    pub bytes: [u8; PERSISTENT_TAIL_SIZE],
    pub len: usize,
}

pub fn take_persistent_log() -> Option<PersistentLogSnapshot> {
    unsafe {
        let p = core::ptr::addr_of_mut!(PERSISTENT_TAIL_RAW).cast::<PersistentTail>();
        let magic = core::ptr::read_volatile(core::ptr::addr_of!((*p).magic));
        if magic != PERSISTENT_TAIL_MAGIC {
            return None;
        }
        let wp_atomic: &AtomicU32 = &*core::ptr::addr_of!((*p).write_pos);
        let wp = wp_atomic.load(Ordering::Acquire) as usize;
        let mut out = [0u8; PERSISTENT_TAIL_SIZE];
        let buf_ptr = core::ptr::addr_of!((*p).buf).cast::<u8>();
        let (start, len) = if wp >= PERSISTENT_TAIL_SIZE {
            (wp - PERSISTENT_TAIL_SIZE, PERSISTENT_TAIL_SIZE)
        } else {
            (0usize, wp)
        };
        for (i, slot) in out.iter_mut().enumerate().take(len) {
            *slot = core::ptr::read_volatile(buf_ptr.add((start + i) % PERSISTENT_TAIL_SIZE));
        }
        // Clear so subsequent calls return None.
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*p).magic), 0);
        Some(PersistentLogSnapshot { bytes: out, len })
    }
}

/// Marks the runtime-drain gate as open so subsequent `log_fmt`
/// (and `info!`/`warn!`) calls land in LOG_RING. Idempotent. Called
/// by `usb::debug_writer_task` on first DTR-assert observation, and
/// by `usb::runtime_drain_open_timeout_task` after 30 s.
pub fn open_runtime_drain() {
    if !RUNTIME_DRAIN_OPEN.swap(true, Ordering::Relaxed) {
        let dropped = RUNTIME_DROPPED.load(Ordering::Relaxed);
        log_fmt_critical(
            "[LOG_GATE] ",
            format_args!("opened, dropped {} runtime lines pre-attach", dropped),
        );
    }
}

// Tracing subscriber
/// Minimal tracing subscriber that routes `leviculum-core` log events
/// to the ring buffer via log_fmt.
pub struct TracingSubscriber;

impl tracing_core::Subscriber for TracingSubscriber {
    fn enabled(&self, metadata: &tracing_core::Metadata<'_>) -> bool {
        metadata.level() <= &tracing_core::Level::DEBUG
    }

    fn new_span(&self, _: &tracing_core::span::Attributes<'_>) -> tracing_core::span::Id {
        tracing_core::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing_core::span::Id, _: &tracing_core::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing_core::span::Id, _: &tracing_core::span::Id) {}
    fn enter(&self, _: &tracing_core::span::Id) {}
    fn exit(&self, _: &tracing_core::span::Id) {}

    fn event(&self, event: &tracing_core::Event<'_>) {
        // Tracing events are runtime-class (leviculum-core's transport
        // layer). Gate them on the RUNTIME_DRAIN_OPEN flag so they
        // don't lap the boot-critical scrollback before the host attaches.
        if !RUNTIME_DRAIN_OPEN.load(Ordering::Relaxed) {
            RUNTIME_DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing_core::Level::ERROR => "ERROR",
            tracing_core::Level::WARN => " WARN",
            tracing_core::Level::INFO => " INFO",
            tracing_core::Level::DEBUG => "DEBUG",
            tracing_core::Level::TRACE => "TRACE",
        };

        // Stamp first, so it is the moment the event happened rather
        // than the moment its (unbounded) field list finished
        // formatting.
        let stamp = uptime_ms();
        let mut buf = [0u8; 1024];
        let mut len = 0usize;

        // The tail of the buffer belongs to the stamp and the CRLF; the
        // prefix and the fields share what is left. Same reservation
        // `format_line` makes for `log_fmt`, and for the same reason: a
        // truncated stamp is a wrong measurement, a truncated field list
        // is only a shorter one.
        {
            let limit = leviculum_log_line::body_limit(buf.len());
            let mut w = leviculum_log_line::Sink::new(&mut buf[..limit], &mut len);
            let _ = core::fmt::Write::write_fmt(
                &mut w,
                format_args!("[{}] {}: ", level, metadata.target()),
            );
        }
        {
            let limit = leviculum_log_line::body_limit(buf.len());
            event.record(&mut TracingVisitor {
                buf: &mut buf[..limit],
                len: &mut len,
            });
        }
        let len = leviculum_log_line::finish(&mut buf, len, stamp);

        LOG_RING.write(&buf[..len]);
        PERSISTENT_TAIL.write(&buf[..len]);
        LOG_SIGNAL.signal(());
    }
}

/// Visitor that formats tracing event fields into a buffer.
struct TracingVisitor<'a> {
    buf: &'a mut [u8],
    len: &'a mut usize,
}

impl tracing_core::field::Visit for TracingVisitor<'_> {
    fn record_debug(&mut self, field: &tracing_core::field::Field, value: &dyn core::fmt::Debug) {
        let mut w = leviculum_log_line::Sink::new(self.buf, self.len);
        if field.name() == "message" {
            let _ = core::fmt::Write::write_fmt(&mut w, format_args!("{:?}", value));
        } else {
            let _ =
                core::fmt::Write::write_fmt(&mut w, format_args!(" {}={:?}", field.name(), value));
        }
    }
}
