//! The **transport** axis: getting the bytes in over UF2 mass storage.
//!
//! Mount the bootloader's drive, read what it says about itself, copy an
//! image on, sync. Three things about this path are not obvious:
//!
//! * **A successful write ends in a kernel error.** The bootloader reboots
//!   the moment the final block lands, while the filesystem still wants to
//!   flush metadata, producing `device offline error ... lost async page
//!   write`. That is the normal completion path
//!   (docs/src/concepts/lnode-flashing.md, "Practical details that bite").
//! * **A copy returning 0 does not mean the flash took.** Nothing here
//!   claims success; [`verify`](crate::verify) decides that.
//! * **Blocks below the writable window are declined silently, with a
//!   success return.** The block counter still sees them arrive, so a
//!   report that counts copied blocks is not evidence that all of them
//!   landed. [`Written::declined`] carries that number rather than hiding it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::infouf2::{self, InfoUf2};
use crate::sys::Mount;
use crate::uf2::Image;

/// What the bootloader publishes about itself.
pub const INFO_FILE: &str = "INFO_UF2.TXT";
/// The whole writable window, dumped as a UF2. Readable without writing
/// anything, which is what lets the SoftDevice version be cross-checked and
/// the previous firmware be backed up.
pub const CURRENT_FILE: &str = "CURRENT.UF2";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("this needs root: the bootloader drive is a root:disk block device")]
    NeedsRoot,
    #[error("the bootloader on {port} exposes no drive to write to")]
    NoDrive { port: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Uf2(#[from] crate::uf2::Error),
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}

/// A mounted bootloader drive.
pub struct Drive {
    mount: Mount,
    /// The temporary mount point, removed when the drive is released.
    _scratch: tempfile::TempDir,
}

impl Drive {
    /// Mount the bootloader's mass-storage device.
    pub fn open(device: &Path) -> Result<Self, Error> {
        if !crate::sys::is_root() {
            return Err(Error::NeedsRoot);
        }
        let scratch = tempfile::Builder::new()
            .prefix("lnflash-")
            .tempdir()
            .map_err(io_err("making a mount point"))?;
        let mount = Mount::vfat(device, scratch.path())
            .map_err(io_err(format!("mounting {}", device.display())))?;
        Ok(Self {
            mount,
            _scratch: scratch,
        })
    }

    pub fn path(&self) -> &Path {
        self.mount.path()
    }

    /// Read and parse `INFO_UF2.TXT`. This is the identity a write is
    /// allowed to rest on, and the only one.
    pub fn info(&self) -> Result<InfoUf2, Error> {
        let path = self.path().join(INFO_FILE);
        let text = read_text(&path)?;
        Ok(infouf2::parse(&text))
    }

    /// Read `CURRENT.UF2`, the bootloader's dump of the writable window.
    pub fn current(&self) -> Result<Image, Error> {
        let path = self.path().join(CURRENT_FILE);
        let bytes = std::fs::read(&path).map_err(io_err(format!("reading {}", path.display())))?;
        Ok(Image::parse(&bytes)?)
    }

    /// Copy an image onto the drive.
    ///
    /// `Ok` means the bytes were handed to the kernel and the drive either
    /// synced or went away mid-flush the way a rebooting bootloader does.
    /// It does not mean the flash took — only [`verify`](crate::verify)
    /// can say that.
    pub fn write_image(
        &self,
        name: &str,
        image: &Image,
        declined: usize,
    ) -> Result<Written, Error> {
        // `encode` refuses a bootloader-family image, so the one write path
        // in the tool cannot emit one.
        let bytes = image.encode()?;
        let path = self.path().join(name);
        let mut reboot_error = None;

        let outcome = (|| -> io::Result<()> {
            let mut file = std::fs::File::create(&path)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()
        })();

        if let Err(err) = outcome {
            if !is_bootloader_reboot(&err) {
                return Err(Error::Io {
                    context: format!("writing {}", path.display()),
                    source: err,
                });
            }
            reboot_error = Some(err.to_string());
        }

        Ok(Written {
            path,
            bytes: bytes.len(),
            blocks: image.blocks.len(),
            declined,
            reboot_error,
        })
    }

    /// Unmount, reporting a failure the caller can act on.
    pub fn close(self) -> Result<(), Error> {
        match self.mount.unmount() {
            Ok(()) => Ok(()),
            Err(err) if is_bootloader_reboot(&err) => Ok(()),
            Err(err) => Err(Error::Io {
                context: "unmounting the bootloader drive".into(),
                source: err,
            }),
        }
    }
}

/// The outcome of a copy — deliberately not called "success".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    pub bytes: usize,
    pub blocks: usize,
    /// Blocks the bootloader will decline because they fall below the
    /// writable window. They are counted in `blocks` and never land.
    pub declined: usize,
    /// The kernel error a rebooting bootloader produces, if it did. Present
    /// on the ordinary happy path.
    pub reboot_error: Option<String>,
}

impl Written {
    /// Blocks that can actually reach flash.
    pub fn accepted(&self) -> usize {
        self.blocks.saturating_sub(self.declined)
    }
}

/// Whether an I/O error is the bootloader rebooting on the final block
/// rather than a real failure.
///
/// The device disappears mid-flush, so the kernel reports the write as
/// failed against a device that is no longer there. Every one of these
/// means "the transfer got far enough that the board restarted".
pub fn is_bootloader_reboot(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        // The drive vanished underneath the write.
        Some(libc::ENODEV) | Some(libc::ENXIO) | Some(libc::EIO)
        // The mount went stale when the USB device detached.
        | Some(libc::ESTALE) | Some(libc::EBUSY)
    )
}

fn read_text(path: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(path).map_err(io_err(format!("reading {}", path.display())))?;
    // The bootloader writes ASCII with CRLF; decoding lossily beats
    // refusing to identify a board over one stray byte.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reboot_error_is_recognised_as_completion_not_failure() {
        // "device offline error ... lost async page write" reaches userspace
        // as one of these.
        for code in [libc::ENODEV, libc::EIO, libc::ENXIO, libc::ESTALE] {
            assert!(
                is_bootloader_reboot(&io::Error::from_raw_os_error(code)),
                "errno {code} should read as the bootloader rebooting"
            );
        }
    }

    #[test]
    fn a_real_failure_is_still_a_failure() {
        for code in [libc::ENOSPC, libc::EACCES, libc::EROFS, libc::ENOENT] {
            assert!(
                !is_bootloader_reboot(&io::Error::from_raw_os_error(code)),
                "errno {code} is a genuine write failure"
            );
        }
        assert!(!is_bootloader_reboot(&io::Error::other("not an errno")));
    }

    #[test]
    fn a_write_reports_declined_blocks_separately_from_copied_ones() {
        // 608 blocks arrive, 11 of them below 0x1000 are declined silently
        // and still counted by the transfer.
        let written = Written {
            path: PathBuf::from("/mnt/x.uf2"),
            bytes: 311_296,
            blocks: 608,
            declined: 11,
            reboot_error: Some("Input/output error (os error 5)".into()),
        };
        assert_eq!(written.accepted(), 597);
        assert_ne!(written.accepted(), written.blocks);
    }

    #[test]
    fn opening_a_drive_without_privileges_says_so_first() {
        if crate::sys::is_root() {
            return;
        }
        assert!(matches!(
            Drive::open(Path::new("/dev/sdz")),
            Err(Error::NeedsRoot)
        ));
    }
}
