# Fields

The `fields` element of the payload (`LXMessage.py:359`) is a msgpack map with
integer keys. Keys are the `FIELD_*` identifiers; values are field-specific. An
empty map `{}` is valid and is the default. Field keys are packed as msgpack
integers, including the high-value debug keys which serialize as `uint8`.

## Field identifiers (`LXMF.py:8-41`)

| Key | Name | Value convention |
|-----|------|------------------|
| 0x01 | `FIELD_EMBEDDED_LXMS` | list of embedded LXM byte strings |
| 0x02 | `FIELD_TELEMETRY` | telemetry blob |
| 0x03 | `FIELD_TELEMETRY_STREAM` | telemetry stream blob |
| 0x04 | `FIELD_ICON_APPEARANCE` | appearance descriptor |
| 0x05 | `FIELD_FILE_ATTACHMENTS` | list of `[name, bytes]` |
| 0x06 | `FIELD_IMAGE` | `[format, bytes]` |
| 0x07 | `FIELD_AUDIO` | `[audio_mode, bytes]` (see audio modes) |
| 0x08 | `FIELD_THREAD` | thread reference |
| 0x09 | `FIELD_COMMANDS` | list of commands |
| 0x0A | `FIELD_RESULTS` | list of results |
| 0x0B | `FIELD_GROUP` | group metadata |
| 0x0C | `FIELD_TICKET` | `[expires, ticket]`, see [Tickets](08-tickets.md) |
| 0x0D | `FIELD_EVENT` | event payload |
| 0x0E | `FIELD_RNR_REFS` | RNR references |
| 0x0F | `FIELD_RENDERER` | renderer hint (see renderers) |
| 0xFB | `FIELD_CUSTOM_TYPE` | custom type tag |
| 0xFC | `FIELD_CUSTOM_DATA` | custom data |
| 0xFD | `FIELD_CUSTOM_META` | custom metadata |
| 0xFE | `FIELD_NON_SPECIFIC` | unspecified |
| 0xFF | `FIELD_DEBUG` | debug payload |

An implementation MUST treat unknown field keys as opaque and preserve them
(the reference round-trips the whole `fields` map through msgpack). `[VEC-MSG-2]`
carries `{0x0F: 0x02}` (`FIELD_RENDERER: RENDERER_MARKDOWN`) and shows it packed
inside the payload.

## Renderers (`LXMF.py:89-92`)

| Value | Name |
|-------|------|
| 0x00 | `RENDERER_PLAIN` |
| 0x01 | `RENDERER_MICRON` |
| 0x02 | `RENDERER_MARKDOWN` |
| 0x03 | `RENDERER_BBCODE` |

## Audio modes (`LXMF.py:55-79`)

Used as the first element of `FIELD_AUDIO`. Codec2 modes `AM_CODEC2_450PWB`
(0x01) through `AM_CODEC2_3200` (0x09); Opus modes `AM_OPUS_OGG` (0x10) through
`AM_OPUS_LOSSLESS` (0x19); `AM_CUSTOM` (0xFF). These identify the audio codec and
profile of an attached clip; LXMF does not process the audio, it only carries the
mode byte and the encoded bytes.
