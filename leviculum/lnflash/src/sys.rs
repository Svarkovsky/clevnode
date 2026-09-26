//! Every `unsafe` block in the crate, in one file.
//!
//! The batch constraint is that nothing outside the bundle is needed: no
//! `stty`, no `udevadm`, no `mount(8)`, no `udisksctl`. What that costs is
//! three syscalls the standard library does not wrap — `tcsetattr`, `ioctl`
//! and `mount(2)` — so they live here behind owned handles, and no other
//! module writes `unsafe`.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn last_error<T>(what: &str) -> io::Result<T> {
    let err = io::Error::last_os_error();
    Err(io::Error::new(err.kind(), format!("{what}: {err}")))
}

/// An owned file descriptor that closes itself.
#[derive(Debug)]
pub struct Fd(libc::c_int);

impl Fd {
    /// Open a serial port the way a flasher has to: no controlling terminal,
    /// and non-blocking so a port with nothing on the other end cannot hang
    /// the open on carrier detect.
    pub fn open_serial(path: &Path) -> io::Result<Self> {
        let c_path = cstring(path)?;
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return last_error(&format!("opening {}", path.display()));
        }
        Ok(Self(fd))
    }

    fn termios(&self) -> io::Result<libc::termios> {
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(self.0, &mut tio) } != 0 {
            return last_error("tcgetattr");
        }
        Ok(tio)
    }

    fn set_termios(&self, tio: &libc::termios) -> io::Result<()> {
        if unsafe { libc::tcsetattr(self.0, libc::TCSANOW, tio) } != 0 {
            return last_error("tcsetattr");
        }
        Ok(())
    }

    /// The 1200-baud touch. The line-coding change is the whole message —
    /// the firmware resets on it — so nothing is written to the port.
    pub fn set_baud_1200(&self) -> io::Result<()> {
        let mut tio = self.termios()?;
        unsafe {
            libc::cfmakeraw(&mut tio);
            if libc::cfsetispeed(&mut tio, libc::B1200) != 0
                || libc::cfsetospeed(&mut tio, libc::B1200) != 0
            {
                return last_error("cfsetspeed");
            }
        }
        self.set_termios(&tio)
    }

    /// Raw 8N1 at 115200 with DTR and RTS asserted, which is how the debug
    /// port is read. **The debug CDC transmits only with DTR+RTS asserted;
    /// without them a healthy board reads as silent** — a false "no banner"
    /// would then be reported as an unverified flash.
    pub fn set_debug_port(&self) -> io::Result<()> {
        self.set_raw_115200_dtr_rts()
    }

    /// The same line settings for the transport CDC (if02), where the radio
    /// configuration is sent. DTR is not cosmetic there either: the firmware's
    /// serial task waits on `cdc.wait_connection()` — DTR — before it reads a
    /// single byte (`leviculum-nrf/src/usb.rs`), so a port opened without it
    /// would swallow the frame.
    pub fn set_transport_port(&self) -> io::Result<()> {
        self.set_raw_115200_dtr_rts()
    }

    fn set_raw_115200_dtr_rts(&self) -> io::Result<()> {
        let mut tio = self.termios()?;
        unsafe {
            libc::cfmakeraw(&mut tio);
            tio.c_cflag |= libc::CLOCAL | libc::CREAD | libc::CS8;
            if libc::cfsetispeed(&mut tio, libc::B115200) != 0
                || libc::cfsetospeed(&mut tio, libc::B115200) != 0
            {
                return last_error("cfsetspeed");
            }
        }
        self.set_termios(&tio)?;
        let bits: libc::c_int = libc::TIOCM_DTR | libc::TIOCM_RTS;
        if unsafe { libc::ioctl(self.0, libc::TIOCMBIS, &bits) } != 0 {
            // ENOTTY is a device with no modem lines to raise — a
            // pseudo-terminal, which is what the scripted-board tests hand
            // in. A real CDC-ACM port supports the ioctl, so anything else
            // is still an error.
            if io::Error::last_os_error().raw_os_error() != Some(libc::ENOTTY) {
                return last_error("ioctl(TIOCMBIS, DTR|RTS)");
            }
        }
        Ok(())
    }

    /// Write every byte, or fail. The descriptor is non-blocking, so a short
    /// write and `EAGAIN` are both ordinary and the loop waits the port out
    /// rather than reporting a failure that did not happen.
    pub fn write_all(&self, data: &[u8], deadline: Instant) -> io::Result<()> {
        let mut sent = 0;
        while sent < data.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("wrote {sent} of {} bytes before the deadline", data.len()),
                ));
            }
            if !self.wait_writable(remaining)? {
                continue;
            }
            let n = unsafe { libc::write(self.0, data[sent..].as_ptr().cast(), data.len() - sent) };
            if n > 0 {
                sent += n as usize;
            } else if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => continue,
                    _ => return Err(err),
                }
            }
        }
        Ok(())
    }

    /// One read, waiting up to `timeout` for something to arrive.
    ///
    /// `None` is end of file — the board went away — which the caller has to
    /// tell apart from "nothing yet", or a reboot mid-wait becomes a spin.
    pub fn read_available(&self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        if !self.wait_readable(timeout)? {
            return Ok(Some(Vec::new()));
        }
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            return Ok(Some(buf[..n as usize].to_vec()));
        }
        if n == 0 {
            return Ok(None);
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(Some(Vec::new())),
            _ => Err(err),
        }
    }

    /// Read whatever arrives until `deadline`, returning the bytes.
    ///
    /// Used to catch a banner the firmware re-emits every few seconds, so
    /// running out of time is an ordinary outcome, not an error.
    pub fn read_until(&self, deadline: Instant, max: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline && out.len() < max {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !self.wait_readable(remaining)? {
                continue;
            }
            let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            } else if n == 0 {
                break;
            } else {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => continue,
                    // The board rebooting mid-read is not a read failure.
                    _ => break,
                }
            }
        }
        Ok(out)
    }

    /// Throw away whatever the board has already said but nobody has
    /// read — the input queue the kernel has buffered for this port.
    ///
    /// Called at the start of every control transaction. A frame sitting
    /// in that queue before a frame of ours has even been written cannot
    /// be an answer to it; it is a leftover from an earlier conversation,
    /// most often a slow board's answer that arrived after our window had
    /// run down and our retry had already gone out. Left in place, the
    /// next transaction reads it as its own answer — and for the media
    /// report, which unlike an ack does not name the frame it answers,
    /// that means a `--set-media` reporting the profile the board was on
    /// *before* the write (#255).
    ///
    /// `TCIFLUSH` rather than a read loop on purpose: one syscall that
    /// cannot spin, where a loop against a board that is talking
    /// continuously (a debug banner on the same port) has no bound.
    pub fn drain_input(&self) -> io::Result<()> {
        if unsafe { libc::tcflush(self.0, libc::TCIFLUSH) } != 0 {
            return last_error("tcflush");
        }
        Ok(())
    }

    /// The device number this descriptor is bound to.
    ///
    /// An open fd stays bound to the driver instance it opened — a device
    /// that re-enumerates afterwards kills the fd with EIO rather than
    /// retargeting it — so comparing this against the node a fresh sysfs
    /// read names is a proof of *which physical port* the fd talks to,
    /// taken after the open and therefore free of the resolve-then-open
    /// race (#334 family).
    pub fn rdev(&self) -> io::Result<u64> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.0, &mut st) } != 0 {
            return last_error("fstat");
        }
        Ok(st.st_rdev)
    }

    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        self.wait(libc::POLLIN, timeout)
    }

    fn wait_writable(&self, timeout: Duration) -> io::Result<bool> {
        self.wait(libc::POLLOUT, timeout)
    }

    fn wait(&self, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.0,
            events,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(ready > 0)
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        // A close that fails has nothing left to report to: the descriptor
        // is gone either way, and on the touch path the port disappearing
        // underneath us is the intended outcome.
        unsafe { libc::close(self.0) };
    }
}

/// A filesystem mounted by us, unmounted when dropped.
///
/// `mount(2)` directly, because `mount(8)` and `udisksctl` are exactly the
/// external dependencies the bundle exists to avoid, and automounting
/// assumes a desktop stack a headless host does not have.
#[derive(Debug)]
pub struct Mount {
    target: std::path::PathBuf,
    active: bool,
}

impl Mount {
    /// Mount `device` at `target` as vfat. Needs root: the mass-storage
    /// device appears as `/dev/sdX` owned `root:disk`.
    pub fn vfat(device: &Path, target: &Path) -> io::Result<Self> {
        let c_device = cstring(device)?;
        let c_target = cstring(target)?;
        let fstype = CString::new("vfat").expect("no NUL in a literal");
        let rc = unsafe {
            libc::mount(
                c_device.as_ptr(),
                c_target.as_ptr(),
                fstype.as_ptr(),
                libc::MS_NOATIME | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOSUID,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            let err = io::Error::last_os_error();
            let hint = if err.kind() == io::ErrorKind::PermissionDenied {
                " (the bootloader drive is root:disk, so this needs sudo)"
            } else {
                ""
            };
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "mounting {} at {}{hint}: {err}",
                    device.display(),
                    target.display()
                ),
            ));
        }
        Ok(Self {
            target: target.to_path_buf(),
            active: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.target
    }

    /// Unmount, reporting failure. Dropping does the same thing silently;
    /// call this when the caller can act on the answer.
    pub fn unmount(mut self) -> io::Result<()> {
        self.unmount_inner()
    }

    fn unmount_inner(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let c_target = cstring(&self.target)?;
        if unsafe { libc::umount2(c_target.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let plain = io::Error::last_os_error();
        // The bootloader reboots on the last block written, so by the time
        // we unmount the device is often already gone. A lazy unmount then
        // detaches the stale mount rather than leaving it behind.
        if unsafe { libc::umount2(c_target.as_ptr(), libc::MNT_DETACH) } == 0 {
            return Ok(());
        }
        Err(io::Error::new(
            plain.kind(),
            format!("unmounting {}: {plain}", self.target.display()),
        ))
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = self.unmount_inner();
    }
}

/// Whether the process can mount and write a `root:disk` block device.
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// The host's UTC offset at `unix_secs`, in seconds, from the C library's
/// local time (which reads the TZ database). Asked per timestamp rather
/// than once, so a DST change during a long watch moves the offset with
/// it. `0` when the conversion fails — the stamp then names the same
/// instant, just written as UTC, which is the honest fallback.
///
/// Lives here because `localtime_r` is a raw libc call; the ISO-8601
/// rendering that consumes this is pure and tested in `watch`.
pub fn utc_offset_secs(unix_secs: i64) -> i64 {
    // The `time_t` deprecation warns about musl going 64-bit; this cast
    // already assumes the 64-bit width, so the rename changes nothing.
    #[allow(deprecated)]
    let t = unix_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; localtime_r writes
    // only into `tm` and returns NULL on failure.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

/// A scripted serial device for tests: a pseudo-terminal whose slave path
/// the code under test opens like a board's transport CDC, while a stub
/// thread plays the device on the master end. Lives here because the pty
/// setup is this crate's only other `unsafe`.
#[cfg(test)]
pub(crate) mod testpty {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};

    pub struct Pty {
        master: File,
        pub slave_path: PathBuf,
        /// Keeps the slave side open so the master never reads EIO between
        /// the test's own opens of the slave path.
        _holder: File,
    }

    impl Pty {
        pub fn open() -> Self {
            // SAFETY: plain pty setup; the master fd ends up in an owned
            // File, the name buffer outlives every read of it.
            let (master, slave_path) = unsafe {
                let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
                assert!(master >= 0, "posix_openpt failed");
                assert_eq!(libc::grantpt(master), 0, "grantpt failed");
                assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
                let mut name = [0 as libc::c_char; 128];
                assert_eq!(
                    libc::ptsname_r(master, name.as_mut_ptr(), name.len()),
                    0,
                    "ptsname_r failed"
                );
                let path = std::ffi::CStr::from_ptr(name.as_ptr())
                    .to_str()
                    .expect("pty path is ASCII")
                    .to_owned();
                // Raw line discipline for the pair: no echo, no canonical
                // buffering — this carries bytes, not a terminal session.
                let mut tio: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(master, &mut tio), 0);
                libc::cfmakeraw(&mut tio);
                assert_eq!(libc::tcsetattr(master, libc::TCSANOW, &tio), 0);
                (File::from_raw_fd(master), PathBuf::from(path))
            };
            let holder = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(&slave_path)
                .expect("opening the pty slave");
            Pty {
                master,
                slave_path,
                _holder: holder,
            }
        }
    }

    impl Pty {
        /// Write raw bytes from the device side, no HDLC framing: what a
        /// board's debug CDC does, a text log rather than a framed
        /// transport. Dropping the `Pty` afterwards closes the master and
        /// the slave reads EOF — but the pty discards bytes still queued
        /// at that moment, so a test must have read everything it wrote
        /// before it drops this.
        pub fn write_raw(&self, data: &[u8]) {
            self.master
                .try_clone()
                .expect("cloning the pty master")
                .write_all(data)
                .expect("writing raw bytes");
        }

        /// Put a frame on the wire from the device side without the
        /// device having been asked for it.
        ///
        /// What a leftover answer from an earlier conversation looks like
        /// to the host: bytes already in the port's input queue when the
        /// next transaction begins.
        pub fn inject(&self, payload: &[u8]) {
            let mut framed = Vec::new();
            frame(payload, &mut framed);
            self.master
                .try_clone()
                .expect("cloning the pty master")
                .write_all(&framed)
                .expect("writing the injected frame");
        }
    }

    /// Run `script` as the device: for every deframed HDLC frame the host
    /// writes, `Some(answer)` is HDLC-framed back, `None` is scripted
    /// silence. The thread ends when the pty goes away with the test.
    pub fn spawn_stub(pty: &Pty, script: impl Fn(&[u8]) -> Option<Vec<u8>> + Send + 'static) {
        spawn_stub_delayed(pty, move |data| {
            script(data)
                .map(|answer| vec![(Duration::ZERO, answer)])
                .unwrap_or_default()
        });
    }

    /// [`spawn_stub`] for a device that answers with more than one frame,
    /// each after its own delay.
    ///
    /// The delay is the point: a leftover answer only survives into the
    /// host's *next* transaction if it arrives after the current one has
    /// been satisfied. Written without one, it would land in the same
    /// read as the answer it trails and be dropped with that
    /// transaction's deframer — which is a different, milder bug than the
    /// one being reproduced.
    pub fn spawn_stub_delayed(
        pty: &Pty,
        script: impl Fn(&[u8]) -> Vec<(Duration, Vec<u8>)> + Send + 'static,
    ) {
        let mut master = pty.master.try_clone().expect("cloning the pty master");
        std::thread::spawn(move || {
            let mut deframer = Deframer::new();
            let mut buf = [0u8; 512];
            loop {
                let n = match master.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for result in deframer.process(&buf[..n]) {
                    if let DeframeResult::Frame(data) = result {
                        for (after, answer) in script(&data) {
                            std::thread::sleep(after);
                            let mut framed = Vec::new();
                            frame(&answer, &mut framed);
                            if master.write_all(&framed).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_port_that_is_not_there_names_the_port() {
        let err = Fd::open_serial(Path::new("/dev/ttyDoesNotExist")).unwrap_err();
        assert!(format!("{err}").contains("/dev/ttyDoesNotExist"), "{err}");
    }

    #[test]
    fn a_path_with_a_nul_byte_is_rejected_before_it_reaches_a_syscall() {
        let bad =
            Path::new(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b"/dev/tty\0ACM0") });
        assert!(Fd::open_serial(bad).is_err());
    }

    #[test]
    fn mounting_without_privileges_says_so_rather_than_just_failing() {
        if is_root() {
            // The suite is not run as root; if it is, this check is vacuous.
            return;
        }
        let target = tempfile::tempdir().unwrap();
        let err = Mount::vfat(Path::new("/dev/null"), target.path()).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("/dev/null") && text.contains(&target.path().display().to_string()));
    }
}
