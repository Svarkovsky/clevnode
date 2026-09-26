#!/usr/bin/env bash
#
# Stack-frame gate for the nRF firmware.
#
# The T114 runs on a 128 KB stack that grows DOWN into the SoftDevice's RAM
# floor (flip-link layout): an overflow past `_stack_end` corrupts SD state
# and surfaces as an SD internal assertion, not as a clean fault. A single
# oversized frame therefore silently eats the whole margin.
#
# That is exactly what happened: `Box::new(builder.build(..))` materialised a
# by-value `NodeCore` (>40 KB with the inline `EmbeddedStorage`) twice in
# `main`'s poll frame — 94 720 B, 74 % of the stack, leaving ~13 KB of margin.
# `NodeCoreBuilder::build_boxed` removed it. This gate keeps it removed.
#
# Reads the frame-allocating `sub sp` immediates straight out of the linked
# ELF, so it measures the shipped binary rather than a source-level proxy.
#
# Usage: check-nrf-stack-frames.sh [max_frame_bytes]

set -euo pipefail

LIMIT="${1:-16384}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NRF="$ROOT/leviculum-nrf"
OUT="$NRF/target/thumbv7em-none-eabihf/release"

# GNU objdump prints the immediate in decimal, llvm-objdump in hex; the
# parser below accepts both, so either tool is fine.
OBJDUMP=""
if command -v arm-none-eabi-objdump >/dev/null 2>&1; then
    OBJDUMP="arm-none-eabi-objdump"
else
    for d in "$(rustc --print sysroot)/lib/rustlib/"*/bin; do
        if [ -x "$d/llvm-objdump" ]; then
            OBJDUMP="$d/llvm-objdump"
            break
        fi
    done
fi
if [ -z "$OBJDUMP" ]; then
    echo "[stack-frames] no objdump found. Install binutils-arm-none-eabi," >&2
    echo "[stack-frames] or add the rustup llvm-tools component." >&2
    exit 1
fi

# stdin: disassembly. stdout: frame sizes in bytes, descending.
parse_frames() {
    python3 -c '
import re, sys
# `sub sp, #N` (narrow) and `sub.w sp, sp, #N` / `subw sp, sp, #N` (wide) are
# the frame-allocating forms LLVM emits for thumbv7em.
pat = re.compile(r"\bsubw?(?:\.w)?\s+sp,\s+(?:sp,\s+)?#(0x[0-9a-fA-F]+|[0-9]+)")
sizes = set()
for line in sys.stdin:
    m = pat.search(line)
    if m:
        sizes.add(int(m.group(1), 0))
if not sizes:
    print("no `sub sp` frames found: objdump output not understood",
          file=sys.stderr)
    sys.exit(2)
for s in sorted(sizes, reverse=True):
    print(s)
'
}

build() {
    # shellcheck disable=SC2086 # feature list is intentionally word-split
    (cd "$NRF" && cargo build --release --bin "$1" --features "$2")
}

check() {
    local bin="$1" elf="$OUT/$1" frames max
    [ -f "$elf" ] || { echo "[stack-frames] missing ELF: $elf" >&2; exit 1; }

    frames="$("$OBJDUMP" -d "$elf" | parse_frames)"
    max="$(printf '%s\n' "$frames" | head -1)"

    if [ "$max" -gt "$LIMIT" ]; then
        echo "[stack-frames] FAIL $bin: largest frame ${max} B > limit ${LIMIT} B"
        echo "[stack-frames] largest frames:"
        printf '%s\n' "$frames" | head -5 | sed 's/^/  /'
        echo "[stack-frames] the owning function is the last symbol header before"
        echo "[stack-frames] the matching 'sub sp' in: $OBJDUMP -d $elf"
        return 1
    fi
    echo "[stack-frames] ok   $bin: largest frame ${max} B (limit ${LIMIT} B)"
}

rc=0
build t114 bsp-t114
build rak4631 bsp-rak4631,rak-baseboard
check t114 || rc=1
check rak4631 || rc=1
exit "$rc"
