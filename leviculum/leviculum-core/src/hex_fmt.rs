/// no_std-compatible hex formatter for tracing output.
pub(crate) struct HexFmt<'a>(pub &'a [u8]);

impl core::fmt::Display for HexFmt<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Hex formatter for hashes in log output: shows the full truncated hash
/// (16 bytes = 32 hex chars for destination hashes).
///
/// Matches Python Reticulum's log format where hashes are shown in full
/// so they can be copy-pasted into tools like `rnprobe` and `rnpath`.
pub(crate) struct HexShort<'a>(pub &'a [u8]);

impl core::fmt::Display for HexShort<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}
