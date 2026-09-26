# Channel and Buffer

A channel is an ordered, typed messaging layer over a link; a buffer is a
byte-stream abstraction built on a channel. This section is proven by
`[VEC-CHAN-ENVELOPE]` and `[VEC-STREAM-HDR]`.

## Channel envelope

Every channel message is wrapped in an envelope (`Channel.Envelope.pack`,
`Channel.py:174-200`):

```
msgtype(u16, big-endian) || sequence(u16, big-endian) || length(u16, big-endian) || payload
```

`[VEC-CHAN-ENVELOPE]`: msgtype `0xabcd`, sequence 7, payload `"channeldata"` packs
to `abcd0007000b6368616e6e656c64617461` — `abcd` type, `0007` sequence, `000b`
length 11, then the payload. Message types `0xF000` and above are reserved for
system messages.

## Stream data message

The buffer layer sends `StreamDataMessage`s (type `SMT_STREAM_DATA = 0xff00`) over
a channel. Each carries a 2-byte header (`Buffer.py:80-92`):

```
header(u16, big-endian):
  bits 0-13  stream_id   (0..STREAM_ID_MAX = 0x3fff)
  bit  14    compressed
  bit  15    eof
then: data
```

`header = (stream_id & 0x3fff) | (0x8000 if eof) | (0x4000 if compressed)`.
`[VEC-STREAM-HDR]`: stream id `0x0102`, eof set, not compressed, payload
`"streamdata"` packs to `810273747265616d64617461` — `8102` header (eof bit set
over stream id 0x0102), then the data.

The combined overhead is 8 bytes (2-byte stream header + 6-byte channel envelope),
so `MAX_DATA_LEN = Link.MDU - 8`. Channel sequencing, acknowledgement, and
retransmission are informative.
