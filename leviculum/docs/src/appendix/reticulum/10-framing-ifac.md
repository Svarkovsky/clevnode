# Framing and IFAC

This section covers how packets are delimited on byte-stream interfaces (HDLC)
and how an interface authenticates and masks packets (IFAC). Both are normative
for wire interop on those media and proven by `[VEC-HDLC]` and `[VEC-IFAC]`.
Medium specifics beyond framing (LoRa airtime, TCP particulars) are informative.

## HDLC byte-stream framing

Byte-stream interfaces (TCP, serial, pipe) delimit packets with HDLC-style flags
and byte stuffing (`Interfaces/TCPInterface.py:44-52,323`):

```
FLAG     = 0x7E
ESC      = 0x7D
ESC_MASK = 0x20

frame = FLAG || escape(packet) || FLAG
escape: replace 0x7D -> 0x7D 0x5D, then 0x7E -> 0x7D 0x5E
```

A literal flag or escape byte inside the packet is replaced by the escape byte
followed by the original XORed with `0x20`. `[VEC-HDLC]`: input `01 7E 02 7D 03`
frames to `7e017d5e027d5d037e` — leading flag, `01`, escaped `7E`→`7D5E`, `02`,
escaped `7D`→`7D5D`, `03`, trailing flag. A receiver un-stuffs by reversing the
replacement between flags. (KISS interfaces use the analogous FEND/FESC framing.)

## IFAC (interface access codes)

An interface configured with a passphrase derives a 64-byte IFAC identity and an
IFAC key, then authenticates and masks every packet (`Transport.transmit`,
`Transport.py:1051-1087`):

```
ifac = ifac_identity.sign(raw)[-ifac_size:]
mask = hkdf(length = len(raw) + ifac_size, derive_from = ifac, salt = ifac_key, context = None)

new_raw  = (raw[0] | 0x80) || raw[1] || ifac || raw[2:]
masked[i] = new_raw[i] ^ mask[i]   for i == 0 (then re-set bit 7), i == 1, and i > ifac_size+1
masked[i] = new_raw[i]             for the ifac bytes (2 .. ifac_size+1, left unmasked)
```

The IFAC flag (header bit 7) is set, the `ifac` tag is inserted right after the
two header bytes, and everything except the tag itself is XOR-masked with the
HKDF stream. On receipt the interface reverses the mask, extracts the tag,
recomputes `ifac_identity.sign(recovered_raw)[-ifac_size:]`, and drops the packet
on mismatch (`Transport.inbound`, `Transport.py:1398-1434`).

`[VEC-IFAC]` (`ifac_size = 8`) records the tag, the mask, the masked output
`9f4a2cf485c1dfcea0…`, the IFAC flag set in the masked header, and proves a full
mask/unmask roundtrip recovers the original packet and a matching tag
(`unmask_roundtrip_ok = true`). `IFAC_MIN_SIZE = 1`, and `IFAC_SALT` is a fixed
32-byte constant (`Reticulum.py:149-150`). An implementation sharing an interface
with Python peers MUST reproduce this masking exactly or its packets are dropped.
