//! Board-specific pin mappings and shared board metadata.

pub mod rak4631;
pub mod t114;

/// Runtime board metadata consumed by shared init code (USB, flash, LoRa).
///
/// Every board module exposes a `pub const CONFIG: BoardConfig` with its
/// own values; shared init functions take `&'static BoardConfig` so the
/// same code path serves every binary target.
pub struct BoardConfig {
    /// USB Vendor ID advertised by the firmware.
    pub usb_vid: u16,
    /// USB Product ID advertised by the firmware.
    pub usb_pid: u16,
    /// USB iManufacturer string.
    pub usb_manufacturer: &'static str,
    /// USB iProduct string.
    pub usb_product: &'static str,
    /// Short tag used in identity log lines (e.g. "T114", "RAK").
    pub log_prefix: &'static str,
    /// Internal-flash byte address of the page reserved for identity storage.
    pub identity_flash_page: u32,
    /// Internal-flash byte address of the page reserved for the persisted
    /// LoRa radio configuration (see [`crate::radio_store`]). Distinct from
    /// [`identity_flash_page`](Self::identity_flash_page) and, like it,
    /// outside the linker's FLASH region (`memory.x`).
    pub radio_config_flash_page: u32,
    /// Internal-flash byte address of the page holding the persisted
    /// telemetry target (Codeberg #236) and the user-set fixed position —
    /// two records, layout named in [`crate::telemetry`]. Third page in
    /// the same band as the two above, chosen for the same reason:
    /// `0xEA000` is the bootloader's `USER_FLASH_END`, so a UF2 flash
    /// declines every block from there upwards and the configuration
    /// survives a firmware update.
    pub telemetry_flash_page: u32,
    /// SX1262 TCXO voltage select byte for `SetDIO3AsTcxoCtrl`
    /// (0x02 = 1.8 V, see datasheet §13.3.6).
    pub lora_tcxo_voltage_reg: u8,
    /// SX1262 SPI bus frequency in Hz.
    pub lora_spi_freq_hz: u32,
    /// SX1262 maximum TX power in dBm.
    pub lora_max_power_dbm: i8,
    /// The QSPI NOR part this board carries: its JEDEC id, its density and
    /// the bus clock it is driven at. The boot probe refuses to hand out a
    /// device whose id does not match this ([`crate::qspi`]).
    pub qspi_part: &'static crate::qspi::FlashPart,
}
