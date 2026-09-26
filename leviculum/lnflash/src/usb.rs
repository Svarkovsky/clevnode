//! USB enumeration by reading sysfs, and nothing else.
//!
//! No `lsusb`, no `udevadm`, no libudev: the point of the bundle is that a
//! stranger needs nothing installed. `/sys/bus/usb/devices/` already carries
//! every fact this tool wants, as plain files.
//!
//! The sysfs root is a parameter rather than a constant, so the enumeration
//! can be pointed at a fixture tree and tested without a board attached.
//!
//! What this module must never be used for is deciding *what a board is*.
//! A USB ID identifies the firmware currently running, and a T114 can arrive
//! carrying Meshtastic, Meshcore, microReticulum, RNode firmware or ours —
//! each with its own ID, and a crashed one with none at all. Enumeration
//! finds candidates; `INFO_UF2.TXT` decides identity
//! (docs/src/concepts/lnode-flashing.md, "Identify").

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Where the kernel publishes USB devices on any Linux.
pub const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// USB interface class for CDC-ACM data/control — the debug and transport
/// ports of a running application.
pub const CLASS_CDC: u8 = 0x02;
/// USB interface class for mass storage — the bootloader's drive.
pub const CLASS_MASS_STORAGE: u8 = 0x08;

/// A `vid:pid` pair, written the way `lsusb` writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsbId {
    pub vid: u16,
    pub pid: u16,
}

impl UsbId {
    pub const fn new(vid: u16, pid: u16) -> Self {
        Self { vid, pid }
    }
}

impl fmt::Display for UsbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vid, self.pid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a vid:pid")]
pub struct BadUsbId;

impl FromStr for UsbId {
    type Err = BadUsbId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (vid, pid) = s.trim().split_once(':').ok_or(BadUsbId)?;
        Ok(Self {
            vid: u16::from_str_radix(vid.trim(), 16).map_err(|_| BadUsbId)?,
            pid: u16::from_str_radix(pid.trim(), 16).map_err(|_| BadUsbId)?,
        })
    }
}

/// One USB interface of a device, with whatever the kernel bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// `bInterfaceNumber` — if00 is our debug port, if02 the transport.
    pub number: u8,
    pub class: u8,
    /// `/dev/<tty>`, when a serial driver claimed the interface.
    pub tty: Option<String>,
    /// `/dev/<block>`, when a storage driver claimed it. `root:disk`, which
    /// is why writing needs root.
    pub block: Option<String>,
}

/// One USB device as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The sysfs name, e.g. `3-2.3.1`. Stable for as long as the device
    /// stays in the same port, and the port is what survives a reboot into
    /// the bootloader.
    pub name: String,
    pub id: UsbId,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub interfaces: Vec<Interface>,
}

impl Device {
    /// The interface with this `bInterfaceNumber`.
    pub fn interface(&self, number: u8) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.number == number)
    }

    /// `/dev/ttyACMn` for the given interface number, if the kernel bound a
    /// serial driver to it.
    pub fn tty(&self, number: u8) -> Option<PathBuf> {
        let tty = self.interface(number)?.tty.as_ref()?;
        Some(Path::new("/dev").join(tty))
    }

    /// The mass-storage block device, if this is a bootloader.
    pub fn block_device(&self) -> Option<PathBuf> {
        let block = self
            .interfaces
            .iter()
            .find_map(|i| i.block.as_ref().filter(|_| i.class == CLASS_MASS_STORAGE))
            .or_else(|| self.interfaces.iter().find_map(|i| i.block.as_ref()))?;
        Some(Path::new("/dev").join(block))
    }

    /// Whether this device is plausibly the same physical board as `other`,
    /// across a reboot between application and bootloader.
    ///
    /// Two independent signals, because neither is sufficient alone. The
    /// serial number changes form between modes — the T114 reports
    /// `183004F712B4A7FE` as an application and `12B4A7FE183004F7` in the
    /// bootloader, the two 32-bit words swapped — and a board with no serial
    /// at all can still be tracked by the USB port it is plugged into.
    pub fn is_same_board(&self, other: &Device) -> bool {
        match (&self.serial, &other.serial) {
            (Some(a), Some(b)) => same_serial(a, b),
            _ => self.port() == other.port(),
        }
    }

    /// The physical port path, `3-2.3.1` without any interface suffix. A
    /// board that reboots into its bootloader keeps it.
    pub fn port(&self) -> &str {
        &self.name
    }
}

/// Whether two serial numbers name the same board, allowing for the
/// application/bootloader word swap.
pub fn same_serial(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
        || swap_words(a).is_some_and(|swapped| swapped.eq_ignore_ascii_case(b))
}

/// Swap the two 32-bit words of a 16-hex-digit serial. `None` for anything
/// that is not that shape, which is the honest answer for a board whose
/// serial we cannot reason about.
pub fn swap_words(serial: &str) -> Option<String> {
    let s = serial.trim();
    if s.len() != 16 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (high, low) = s.split_at(8);
    Some(format!("{low}{high}"))
}

/// A sysfs tree to enumerate. Real at `/sys/bus/usb/devices`, a fixture
/// directory in tests. Carries the `/dev` root beside it because the two
/// answer one question — "where is this board's port" — and a test that can
/// point the first at a fixture must be able to point the second at one too.
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
    dev: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Self::new(SYSFS_USB_DEVICES)
    }
}

impl Sysfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            dev: PathBuf::from("/dev"),
        }
    }

    /// A tree with its device nodes somewhere other than `/dev`. For tests:
    /// the fixture bus stays a directory of files, and the ttys it names can
    /// be symlinks to pseudo-terminals instead of somebody's real board.
    pub fn with_dev(root: impl Into<PathBuf>, dev: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            dev: dev.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The device node for a tty name the bus reported.
    pub fn dev_path(&self, tty_name: &str) -> PathBuf {
        self.dev.join(tty_name)
    }

    /// The stable path for a tty name: the `/dev/serial/by-id` symlink that
    /// currently resolves to it, or the plain node if udev provides none.
    ///
    /// The number in `ttyACMn` names an allocation slot, not a board — a
    /// DTR reset re-enumerates the device and the freed number is handed to
    /// whatever asks next. A by-id link is named after the device's own
    /// identity, so udev re-points it across a re-enumeration and an open
    /// through it lands on the same physical board, or fails honestly with
    /// a missing file (#334 family; project rule: by-id, never bare
    /// numbers).
    pub fn stable_tty_path(&self, tty_name: &str) -> PathBuf {
        let by_id = self.dev.join("serial/by-id");
        let mut links: Vec<PathBuf> = std::fs::read_dir(&by_id)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        links.sort();
        for link in links {
            let points_here = std::fs::read_link(&link)
                .ok()
                .is_some_and(|target| target.file_name().is_some_and(|name| name == tty_name));
            if points_here {
                return link;
            }
        }
        self.dev.join(tty_name)
    }

    /// Every USB device in the tree, in sysfs name order.
    pub fn devices(&self) -> io::Result<Vec<Device>> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)?
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            // Interface directories carry a colon; device directories do not.
            .filter(|name| !name.contains(':'))
            // A hub port with nothing in it has no idVendor.
            .filter(|name| self.root.join(name).join("idVendor").is_file())
            .collect();
        names.sort();
        Ok(names.iter().map(|name| self.device(name)).collect())
    }

    /// Devices matching any of `ids`. The manifest supplies the list, so no
    /// USB ID is written down in code.
    pub fn devices_matching(&self, ids: &[UsbId]) -> io::Result<Vec<Device>> {
        Ok(self
            .devices()?
            .into_iter()
            .filter(|d| ids.contains(&d.id))
            .collect())
    }

    fn device(&self, name: &str) -> Device {
        let dir = self.root.join(name);
        Device {
            name: name.to_string(),
            id: UsbId {
                vid: attr(&dir, "idVendor").and_then(|v| hex16(&v)).unwrap_or(0),
                pid: attr(&dir, "idProduct").and_then(|v| hex16(&v)).unwrap_or(0),
            },
            serial: attr(&dir, "serial"),
            manufacturer: attr(&dir, "manufacturer"),
            product: attr(&dir, "product"),
            interfaces: self.interfaces(name),
        }
    }

    fn interfaces(&self, device: &str) -> Vec<Interface> {
        // Interfaces are siblings of the device, named `<device>:<cfg>.<n>`.
        let prefix = format!("{device}:");
        let mut found: Vec<Interface> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with(&prefix))
            .filter_map(|name| {
                let dir = self.root.join(&name);
                Some(Interface {
                    number: hex8(&attr(&dir, "bInterfaceNumber")?)?,
                    class: hex8(&attr(&dir, "bInterfaceClass")?)?,
                    tty: first_entry(&dir.join("tty")),
                    block: find_block(&dir, 5),
                })
            })
            .collect();
        found.sort_by_key(|i| i.number);
        found
    }
}

/// Read a sysfs attribute. Trailing newline stripped; empty reads as absent,
/// which is what an unpopulated attribute means.
fn attr(dir: &Path, name: &str) -> Option<String> {
    let value = std::fs::read_to_string(dir.join(name)).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn hex16(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

fn hex8(s: &str) -> Option<u8> {
    // `bInterfaceNumber` and `bInterfaceClass` are hex; `bNumInterfaces` is
    // decimal, which is why only the hex ones go through here.
    u8::from_str_radix(s.trim(), 16).ok()
}

fn first_entry(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Walk down to the `block/` directory the SCSI stack hangs off a
/// mass-storage interface: `<iface>/host4/target4:0:0/4:0:0:0/block/sdb`.
/// Depth-limited, because sysfs has symlink cycles and we are looking for
/// something exactly four levels down.
fn find_block(dir: &Path, depth: usize) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    children.sort();
    for child in children {
        if child.file_name().is_some_and(|n| n == "block") {
            if let Some(name) = first_entry(&child) {
                return Some(name);
            }
        } else if child
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("host") || n.starts_with("target") || is_scsi(n))
        {
            if let Some(found) = find_block(&child, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// `4:0:0:0` — a SCSI device address.
fn is_scsi(name: &str) -> bool {
    let parts: Vec<&str> = name.split(':').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u32>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Sysfs {
        Sysfs::new(crate::sysfs_fixture::materialized())
    }

    #[test]
    fn a_vid_pid_round_trips_through_the_form_the_manifest_writes() {
        let id: UsbId = "239a:0071".parse().unwrap();
        assert_eq!(id, UsbId::new(0x239a, 0x0071));
        assert_eq!(id.to_string(), "239a:0071");
        assert_eq!(
            "1209:0001".parse::<UsbId>().unwrap().to_string(),
            "1209:0001"
        );
        assert!("239a-0071".parse::<UsbId>().is_err());
        assert!("zzzz:0001".parse::<UsbId>().is_err());
    }

    #[test]
    fn the_fixture_tree_enumerates_every_device_and_no_interfaces() {
        let names: Vec<String> = fixture()
            .devices()
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(
            names,
            vec!["3-2", "3-2.3", "3-2.3.1", "3-2.3.4.4", "3-2.4", "usb3"],
            "a device dir has no colon in its name and does have an idVendor"
        );
    }

    #[test]
    fn our_application_is_found_with_its_debug_and_transport_ports() {
        let device = fixture()
            .devices_matching(&[UsbId::new(0x1209, 0x0001)])
            .unwrap()
            .remove(0);
        assert_eq!(device.name, "3-2.3.1");
        assert_eq!(device.serial.as_deref(), Some("183004F712B4A7FE"));
        assert_eq!(device.product.as_deref(), Some("leviculum T114"));
        // if00 is the debug log, if02 the Reticulum transport.
        assert_eq!(device.tty(0).unwrap(), Path::new("/dev/ttyACM1"));
        assert_eq!(device.tty(2).unwrap(), Path::new("/dev/ttyACM2"));
        assert_eq!(device.interface(0).unwrap().class, CLASS_CDC);
        assert_eq!(device.interfaces.len(), 4);
        assert_eq!(device.block_device(), None);
    }

    #[test]
    fn the_bootloader_is_found_with_its_mass_storage_drive() {
        let device = fixture()
            .devices_matching(&[UsbId::new(0x239a, 0x0071)])
            .unwrap()
            .remove(0);
        assert_eq!(device.name, "3-2.4");
        assert_eq!(device.product.as_deref(), Some("HT-n5262"));
        assert_eq!(device.interfaces[0].class, CLASS_MASS_STORAGE);
        assert_eq!(device.block_device().unwrap(), Path::new("/dev/sdb"));
        // A bootloader has no serial port to touch.
        assert_eq!(device.tty(0), None);
    }

    #[test]
    fn a_second_board_on_the_bus_is_a_separate_device() {
        // Several devices attached must each be resolved individually rather
        // than assuming "the one UF2 drive".
        let all = fixture().devices().unwrap();
        let ours: Vec<&Device> = all.iter().filter(|d| d.id.vid == 0x1209).collect();
        assert_eq!(ours.len(), 2, "a T114 and a RAK4631, both running ours");
        assert_ne!(ours[0].id, ours[1].id);
        assert_ne!(ours[0].serial, ours[1].serial);
    }

    #[test]
    fn the_application_and_bootloader_serials_are_recognised_as_one_board() {
        // The measured pair: 183004F712B4A7FE as an application,
        // 12B4A7FE183004F7 in the bootloader. The two 32-bit words swap.
        let sysfs = fixture();
        let app = sysfs
            .devices_matching(&[UsbId::new(0x1209, 0x0001)])
            .unwrap()
            .remove(0);
        let boot = sysfs
            .devices_matching(&[UsbId::new(0x239a, 0x0071)])
            .unwrap()
            .remove(0);
        assert_eq!(app.serial.as_deref(), Some("183004F712B4A7FE"));
        assert_eq!(boot.serial.as_deref(), Some("12B4A7FE183004F7"));
        assert_ne!(app.serial, boot.serial, "a plain comparison would miss it");
        assert!(app.is_same_board(&boot));
        assert!(boot.is_same_board(&app));
    }

    #[test]
    fn the_other_board_is_not_mistaken_for_the_one_that_rebooted() {
        let sysfs = fixture();
        let other = sysfs
            .devices_matching(&[UsbId::new(0x1209, 0x0002)])
            .unwrap()
            .remove(0);
        let boot = sysfs
            .devices_matching(&[UsbId::new(0x239a, 0x0071)])
            .unwrap()
            .remove(0);
        assert!(!other.is_same_board(&boot));
    }

    #[test]
    fn the_word_swap_is_its_own_inverse_and_declines_anything_else() {
        assert_eq!(
            swap_words("183004F712B4A7FE").as_deref(),
            Some("12B4A7FE183004F7")
        );
        assert_eq!(
            swap_words(&swap_words("183004F712B4A7FE").unwrap()).as_deref(),
            Some("183004F712B4A7FE")
        );
        assert_eq!(swap_words("short"), None);
        assert_eq!(swap_words("nothexnothexnothe"), None);
        assert!(same_serial("183004f712b4a7fe", "12B4A7FE183004F7"));
        assert!(!same_serial("DEC9947DAD9D2869", "12B4A7FE183004F7"));
    }

    #[test]
    fn a_board_with_no_serial_is_tracked_by_the_port_it_is_plugged_into() {
        let sysfs = fixture();
        let hub = sysfs
            .devices_matching(&[UsbId::new(0x0bda, 0x5411)])
            .unwrap()
            .remove(0);
        assert_eq!(hub.serial, None);
        assert!(hub.is_same_board(&hub.clone()));
    }

    #[test]
    fn a_tty_is_addressed_through_its_by_id_link_when_udev_offers_one() {
        // The rig lesson behind the rule: ttyACM numbers are allocation
        // slots, reused across re-enumerations, so a session that resolved
        // a number and opens it later can land on a different board. The
        // by-id link carries the board's identity in its name and follows
        // the board across a re-enumeration.
        let dev = tempfile::tempdir().unwrap();
        let by_id = dev.path().join("serial/by-id");
        std::fs::create_dir_all(&by_id).unwrap();
        std::os::unix::fs::symlink(
            "../../ttyACM2",
            by_id.join("usb-leviculum_T114_183004F712B4A7FE-if02"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../ttyACM4",
            by_id.join("usb-leviculum_RAK4631_DEC9947DAD9D2869-if02"),
        )
        .unwrap();

        let sysfs = Sysfs::with_dev(crate::sysfs_fixture::materialized(), dev.path());
        assert_eq!(
            sysfs.stable_tty_path("ttyACM2"),
            by_id.join("usb-leviculum_T114_183004F712B4A7FE-if02"),
            "the link that resolves to this tty, not any link"
        );
        assert_eq!(
            sysfs.stable_tty_path("ttyACM4"),
            by_id.join("usb-leviculum_RAK4631_DEC9947DAD9D2869-if02")
        );
        // A tty no link points at falls back to the plain node.
        assert_eq!(sysfs.stable_tty_path("ttyACM9"), dev.path().join("ttyACM9"));
    }

    #[test]
    fn a_host_without_udev_gets_the_plain_node_rather_than_nothing() {
        // Containers and minimal initramfs environments have no
        // /dev/serial/by-id at all; the session still has to run there.
        let dev = tempfile::tempdir().unwrap();
        let sysfs = Sysfs::with_dev(crate::sysfs_fixture::materialized(), dev.path());
        assert_eq!(sysfs.stable_tty_path("ttyACM2"), dev.path().join("ttyACM2"));
    }

    #[test]
    fn an_absent_sysfs_tree_is_an_error_rather_than_an_empty_bus() {
        // "No devices" and "I could not look" must not be the same answer:
        // the second would let the flow report a clean bus on a broken host.
        assert!(Sysfs::new("/nonexistent/sysfs/root").devices().is_err());
    }
}
