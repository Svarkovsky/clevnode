/* nRF52840 memory layout — Adafruit-style bootloader with SoftDevice S140 v7.3.0 */
/* NOTE: FLASH ORIGIN must match --base in tools/uf2-runner.sh */
/* Bug32-softdevice-spike Day 3: bumped from v6.1.1 to v7.3.0 to match    */
/* nrf-softdevice's supported S140 family. Both Pocket V2 and T114 ship   */
/* with v6.1.1 from factory; the spike installs v7.3.0 once via UF2-mass- */
/* storage flash. Old (v6.1.1) numbers in comments for reference.         */
MEMORY
{
    /* Application starts after SoftDevice S140 v7.3.0 at 0x27000 (156K).    */
    /* (Was v6.1.1 at 0x26000=152K.)                                         */
    /* Heltec reserves 0xED000-0xF4000 (28K) for license/version data        */
    /* (HARD_VERSION_ADDR, HT_LICENSE_ADDR in variant.h). Bootloader at      */
    /* 0xF4000. The bootloader's own USER_FLASH_END is 0xEA000: it declines  */
    /* every block at or above that address, so 0xEA000-0xF4000 survives a   */
    /* UF2 flash untouched (docs/src/concepts/lnode-flashing.md:139-167).    */
    /* All persistence pages live in that band and therefore survive a       */
    /* firmware update:                                                      */
    /*   0xEC000  identity          (BoardConfig::identity_flash_page)       */
    /*   0xEB000  radio config      (BoardConfig::radio_config_flash_page)   */
    /*   0xEA000  telemetry target (+0x000, #236), user-set fixed           */
    /*            position (+0x100) and media profile (+0x200) — layout    */
    /*            in leviculum_nrf::telemetry, whose compile-time           */
    /*            assertion checks the three records do not overlap         */
    /*            (BoardConfig::telemetry_flash_page)                        */
    /* 0xEA000 is USER_FLASH_END itself: the bootloader declines every block  */
    /* AT or above it, so the page is the lowest one still safe from a UF2.  */
    /* Safe app space = the bootloader's window, 0xEA000 - 0x27000 =         */
    /* 0xC3000 (780K). Was 0xC5000 (788K), which promised 8K the bootloader  */
    /* would have refused to write and reached into both pages above.        */
    FLASH : ORIGIN = 0x00027000, LENGTH = 0xC3000

    /*
     * RETAINED holds the cross-boot records (boot-trace breadcrumbs,
     * panic counter, panic/hardfault post-mortems, persistent log
     * tail — everything `#[link_section = ".retained"]`). They used to
     * sit in `.uninit`, which flip-link packs against the TOP of RAM —
     * and the Adafruit nRF52 bootloader, which every reset passes
     * through before the app runs, starts its own stack exactly there:
     * its linker script (Adafruit_nRF52_Bootloader linker/nrf_common.ld)
     * sets `__StackTop = ORIGIN(RAM) + LENGTH(RAM)` with RAM ending at
     * 0x20040000 (linker/nrf52840.ld), and the shipped RAK4631
     * bootloader binary's vector table confirms initial SP =
     * 0x20040000. The bootloader stack therefore clobbered the top of
     * `.uninit` on every boot — the rig showed `prev_magic=absent`
     * across commanded resets that provably came from a running system.
     *
     * The bootloader's OWN memory map is what makes this band safe: its
     * RAM region is [0x20008000, 0x20040000) and the only addresses it
     * touches below that are the double-reset word at 0x20007F7C and
     * its NOINIT block at [0x20007F80, 0x20008000) (linker/nrf52840.ld:
     * DBL_RESET and NOINIT regions). [0x20003FC0, 0x20007F7C) is
     * touched by neither the bootloader nor — below the probed ceiling
     * documented for RAM ORIGIN below — the SoftDevice. The one caveat:
     * in OTA-DFU mode the bootloader enables the SD itself, whose RAM
     * then reaches up to 0x20008000; after a DFU the image changed and
     * the records are honestly `absent` anyway.
     *
     * LENGTH is sized to current content (0xC48 as of 2026-08-31) plus
     * a little headroom; growing a record past it is a LINK ERROR, and
     * the answer is to bump LENGTH and RAM ORIGIN here in lockstep.
     * RAM ORIGIN must equal ORIGIN(RETAINED) + LENGTH(RETAINED),
     * 32-byte aligned — the ASSERTs below hold both edges.
     */
    RETAINED : ORIGIN = 0x20006140, LENGTH = 0xC80

    /*
     * RAM ORIGIN is, under flip-link, our stack's FLOOR (`_stack_end`).
     * The SoftDevice's ceiling is ORIGIN(RETAINED) just above — RETAINED
     * sits between the SD and the stack. SD sizing is measured with
     * `src/bin/sd-ram-probe.rs`:
     * `sd_ble_enable` against a deliberately undersized base answers
     * NRF_ERROR_NO_MEM and writes the exact required base back.
     *
     * 2026-05-02, RAK4631 (Pocket V2), the config that shipped then:
     *     wanted_app_ram_base = 0x20002CE0  (11 488 B of SD RAM)
     *     + 0x400 margin      = 0x200030E0  <- the previous ORIGIN
     *
     * 2026-08-29, #255 T2 probe, case "c1" — conn_count 2, periph 1,
     * central 1, att_mtu 256, event_length 24, i.e. the phase-B
     * configuration (phone + one neighbour LNode):
     *     wanted_app_ram_base = 0x20003BA8  (15 272 B of SD RAM)
     *
     * 2026-09-08, #372 — conn_count 4, periph 3, central 1 (three
     * incoming links + one initiated), same att_mtu/event_length.
     * MEASURED on the rig T114 (SD_RAM_FLOOR at boot, firmware
     * ead0bce, 2026-09-08 23:07, rig-run/proof-372-t114.log):
     *     wanted_app_ram_base = 0x20005DA0  (23 968 B of SD RAM)
     *     + 928 B margin      = 0x20006140
     *         <- the SD ceiling = ORIGIN(RETAINED); ORIGIN(RAM) is that
     *            plus LENGTH(RETAINED)
     * (The pre-flash extrapolation from the two points above — 3 784 B
     * per extra connection, 22 840 B total — undershot by 1 128 B: a
     * peripheral slot costs more than c1's central-slot delta.)
     *
     * The measured margin is 928 B, less than the usual 0x400, and it
     * does not need to be more: the requirement is a fixed,
     * deterministic property of this exact configuration, re-measured
     * on every boot — `assert_sd_fits_below_retained` (src/ble/mod.rs)
     * probes the real config and panics with both values if it does
     * not fit, and its SD_RAM_FLOOR log line carries the true
     * requirement. The margin only has to absorb a deliberate config
     * change, and the boot assert catches one that outgrows it.
     * `src/bin/sd-ram-probe.rs` cases p2c1/p3c1 measure the curve on a
     * spare board.
     *
     * Cost of #372: ORIGIN moves up by
     *     0x20006140 - 0x20003FC0 = 0x2180 = 8 576 B
     * and the stack region — [_stack_end, __sdata), everything below
     * .data — shrinks by exactly that. Against the measured stack
     * floor that is cheap: the T114 long-run [STACK] watermark reads
     * min_free ~= 82 900 of a 115 552-byte region (proof-370/-373 rig
     * logs, 2026-09-08), so ~74 KiB of never-touched stack remain
     * after this move.
     *
     * Lower bound: S140 v7 reserves the bottom 8 KiB (0x20000000-
     * 0x20001FFF) for MBR + master-init scratch; cannot probe below
     * that via deliberate undersize.
     *
     * Note: 0x20010000 (64 KiB reservation) regresses with a
     * peripheral-register MEMACC fault (info != 0); root cause now
     * believed to be a layout-driven re-trigger of the same
     * SoftDevice-PREGION violation that caused Bug #32 (direct RNG
     * register access from RawHwRng, fixed in commit f093099). Worth
     * re-running 64 KiB with the f093099 build to confirm.
     *
     * Leaves 256K - 24.3K SD - 3.1K retained = 228.6K (0x39240) for
     * application: 0x20006DC0 = 0x20006140 + 0xC80 (RETAINED), and
     * 0x20006DC0 + 0x39240 = 0x20040000, the top of RAM.
     */
    RAM   : ORIGIN = 0x20006DC0, LENGTH = 0x39240
}

/* The retained section itself. NOLOAD: no image content, and neither
 * cortex-m-rt's startup (which touches only .data/.bss) nor the flash
 * image knows it exists — RAM contents ride through resets untouched.
 * `__sretained` is the SD ceiling `assert_sd_fits_below_retained`
 * (src/ble/mod.rs) checks against at boot. */
SECTIONS
{
    .retained (NOLOAD) : ALIGN(4)
    {
        *(.retained .retained.*);
    } > RETAINED
}

/* Top-level (not inside the section) so the symbols exist even in a
 * binary that keeps nothing retained, e.g. src/bin/sd-ram-probe.rs,
 * whose fit verdict compares against __sretained. */
__sretained = ORIGIN(RETAINED);
__eretained = ORIGIN(RETAINED) + LENGTH(RETAINED);

/* The band is only bootloader-safe below the double-reset word; and the
 * stack floor (ORIGIN(RAM)) must sit on top of RETAINED, or the stack
 * sweeps the records (edge held against future edits of either line). */
ASSERT(ORIGIN(RETAINED) + LENGTH(RETAINED) <= 0x20007F7C,
       "RETAINED reaches into the bootloader's DBL_RESET/NOINIT band (>= 0x20007F7C)");
ASSERT(ORIGIN(RAM) >= ORIGIN(RETAINED) + LENGTH(RETAINED),
       "app RAM (the flip-link stack floor) must start at or above the end of RETAINED");
