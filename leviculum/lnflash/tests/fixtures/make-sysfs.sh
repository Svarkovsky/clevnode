#!/usr/bin/env bash
# Regenerate tests/fixtures/sysfs — a stand-in for /sys/bus/usb/devices.
#
# The tree is committed, not generated at test time, because its shape is
# evidence: it is what the rig host actually publishes, transcribed. A test
# that built its own tree would only prove the enumeration agrees with the
# author's idea of sysfs. Values below were read off the rig on 2026-08-09.
#
# Real sysfs uses symlinks into /sys/devices/; plain directories read
# identically through the same `read_dir` + attribute reads, and survive a
# git checkout.
set -euo pipefail
cd "$(dirname "$0")"
rm -rf sysfs
mkdir -p sysfs

dev() { # <name> <vid> <pid> [serial] [manufacturer] [product]
    local d="sysfs/$1"
    mkdir -p "$d"
    printf '%s\n' "$2" > "$d/idVendor"
    printf '%s\n' "$3" > "$d/idProduct"
    # An absent attribute and an empty one mean the same thing to the reader;
    # a hub with no serial gets no file, the way sysfs would have it.
    if [ -n "${4:-}" ]; then printf '%s\n' "$4" > "$d/serial"; fi
    if [ -n "${5:-}" ]; then printf '%s\n' "$5" > "$d/manufacturer"; fi
    if [ -n "${6:-}" ]; then printf '%s\n' "$6" > "$d/product"; fi
}

iface() { # <device> <cfg.n> <bInterfaceNumber> <bInterfaceClass>
    local d="sysfs/$1:$2"
    mkdir -p "$d"
    printf '%s\n' "$3" > "$d/bInterfaceNumber"
    printf '%s\n' "$4" > "$d/bInterfaceClass"
}

tty() { mkdir -p "sysfs/$1/tty/$2"; }
# The SCSI chain the kernel hangs off a mass-storage interface.
block() { mkdir -p "sysfs/$1/host$2/target$2:0:0/$2:0:0:0/block/$3"; }

# A root hub, so enumeration has something uninteresting to walk past.
dev usb3 1d6b 0002 "0000:00:14.0" "Linux 6.12.101+deb13-amd64 xhci-hcd" "xHCI Host Controller"
iface usb3 1.0 00 09

# The hub the boards hang off. No serial: tracked by port, not by serial.
dev 3-2.3 0bda 5411 "" "Generic" "4-Port USB 2.0 Hub"
iface 3-2.3 1.0 00 09

# Our application on a T114: 1209:0001, if00 debug, if02 transport.
dev 3-2.3.1 1209 0001 183004F712B4A7FE "leviculum" "leviculum T114"
iface 3-2.3.1 1.0 00 02; tty "3-2.3.1:1.0" ttyACM1
iface 3-2.3.1 1.1 01 0a
iface 3-2.3.1 1.2 02 02; tty "3-2.3.1:1.2" ttyACM2
iface 3-2.3.1 1.3 03 0a

# Our application on a RAK4631: same firmware, different board, 1209:0002.
# Present so "resolve each device individually" has something to get wrong.
dev 3-2.3.4.4 1209 0002 DEC9947DAD9D2869 "leviculum" "leviculum RAK4631"
iface 3-2.3.4.4 1.0 00 02; tty "3-2.3.4.4:1.0" ttyACM3
iface 3-2.3.4.4 1.1 01 0a
iface 3-2.3.4.4 1.2 02 02; tty "3-2.3.4.4:1.2" ttyACM4
iface 3-2.3.4.4 1.3 03 0a

# The same T114 as above, in its bootloader: different USB ID, and the
# serial's two 32-bit words swapped (183004F712B4A7FE -> 12B4A7FE183004F7).
dev 3-2.4 239a 0071 12B4A7FE183004F7 "Adafruit Industries" "HT-n5262"
iface 3-2.4 1.0 00 08; block "3-2.4:1.0" 4 sdb

# An empty hub port: a directory with no idVendor, which must not enumerate.
mkdir -p sysfs/3-2/power
dev 3-2 0bda 5423 "" "Generic" "4-Port USB 3.0 Hub"
mkdir -p sysfs/3-2.9

# git does not track empty directories; sysfs attribute files fill every
# directory that matters, except the tty/block leaves.
find sysfs -type d -empty -exec touch {}/.keep \;

# NTFS cannot represent ':' in a path, so a verbatim tree breaks git checkout
# (and cargo git-dependency fetches) on Windows. Commit the tree with ':'
# encoded as '+'; tests decode it back via tests/fixtures/materialize.rs.
find sysfs -depth -name '*:*' | while read -r p; do
    mv "$p" "${p%/*}/$(basename "$p" | tr ':' '+')"
done
echo "wrote $(find sysfs -type f | wc -l) files under $(pwd)/sysfs"
