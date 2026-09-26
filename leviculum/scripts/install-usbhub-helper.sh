#!/bin/bash
# install-usbhub-helper.sh — idempotent installer for the leviculum CI
# usbhub-helper.  Run once on hamster, manually, by Lew.
#
# Copies scripts/usbhub-helper to /usr/local/bin/usbhub-helper, verifies
# the prerequisites (uhubctl present, passwordless sudo for it works),
# and reports any device-keys whose expected USB-VID:PID-Serial signature
# is missing from the current `lsusb` output.
#
# Re-runnable: re-installation overwrites /usr/local/bin/usbhub-helper
# with the current version; verification steps are read-only.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HELPER_SRC="$SCRIPT_DIR/usbhub-helper"
HELPER_DST=/usr/local/bin/usbhub-helper

# device-key → expected USB-VID:PID + iSerial.  Used for a lsusb sanity
# check at install time — does not abort on missing devices, since they
# may legitimately be unplugged (Lew shuffles hardware).
EXPECTED_DEVICES=(
    "t-beam-1:1a86:55d4:5896004228"
    "t-beam-2:1a86:55d4:5896000022"
    "pocket-v2:1209:0002:ABFAB3F1807E459B"
    "heltec-v4:303a:1001:44:1B:F6:69:77:24"
    "t114:1209:0001:DEC9947DAD9D2869"
)

echo "[install-usbhub-helper] checking prerequisites..."

# 1. uhubctl present
if ! command -v uhubctl >/dev/null 2>&1; then
    cat >&2 <<'EOF'
ERROR: uhubctl is not installed.
  Install with: sudo apt install uhubctl
EOF
    exit 1
fi
echo "  uhubctl: $(command -v uhubctl)"

# 2. Passwordless sudo for uhubctl works.  Check the effective behaviour
# (sudo -n returns 0) rather than parsing the sudoers file — more robust
# against unrelated sudoers drift.
if ! sudo -n /usr/sbin/uhubctl >/dev/null 2>&1; then
    cat >&2 <<'EOF'
ERROR: passwordless sudo for /usr/sbin/uhubctl is not configured.
Create the rule with:

  sudo tee /etc/sudoers.d/uhubctl <<'SUDOERS'
  lew ALL=(root) NOPASSWD: /usr/sbin/uhubctl
  SUDOERS
  sudo chmod 0440 /etc/sudoers.d/uhubctl
  sudo visudo -c -f /etc/sudoers.d/uhubctl

Then re-run install-usbhub-helper.sh.
EOF
    exit 1
fi
echo "  passwordless sudo for uhubctl: ok"

# 3. Helper source exists
if [[ ! -f "$HELPER_SRC" ]]; then
    echo "ERROR: $HELPER_SRC not found.  Run from a checkout of libreticulum." >&2
    exit 1
fi

# 4. Install
echo "[install-usbhub-helper] installing $HELPER_DST"
sudo install -m 0755 "$HELPER_SRC" "$HELPER_DST"
echo "  $HELPER_DST: $(stat -c '%A %U %G %s' "$HELPER_DST")"

# 5. lsusb sanity check.  Warns on missing devices, never aborts.
echo "[install-usbhub-helper] checking expected devices visible in lsusb..."
LSUSB_OUT=$(lsusb)
MISSING=()
for entry in "${EXPECTED_DEVICES[@]}"; do
    # Entry format: "key:vid:pid:serial".  Heltec V4's serial contains
    # colons (MAC-style), so we grab the first three colon-fields and
    # treat the rest as the serial.
    key=${entry%%:*}
    rest=${entry#*:}
    vid=${rest%%:*}
    rest=${rest#*:}
    pid=${rest%%:*}
    if echo "$LSUSB_OUT" | grep -q "$vid:$pid"; then
        echo "  ok: $key ($vid:$pid)"
    else
        echo "  MISSING: $key ($vid:$pid) not in lsusb (device unplugged?)"
        MISSING+=("$key")
    fi
done

echo ""
if [[ ${#MISSING[@]} -gt 0 ]]; then
    echo "[install-usbhub-helper] installation OK; ${#MISSING[@]} device(s) currently absent: ${MISSING[*]}"
else
    echo "[install-usbhub-helper] installation OK; all expected devices visible."
fi
echo ""
echo "Smoke-test from schneckenschreck (after authorized_keys is set up):"
echo "  ssh hamster status"
