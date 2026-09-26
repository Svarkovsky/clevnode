# Tickets

A ticket is a 16-byte shared secret (`TICKET_LENGTH`, `LXMessage.py:42`) that lets
a known correspondent skip proof-of-work. The recipient issues a ticket to a
sender; the sender then derives stamps from it cheaply.

## Derivation

A ticketed stamp is (`LXMessage.py:297`, validated at `:274`):

```
stamp = truncated_hash(ticket || message_id)
```

with value `COST_TICKET = 256` (`LXMessage.py:52,298`). On the receiving side,
`validate_stamp` accepts the message if `stamp` equals
`truncated_hash(ticket || message_id)` for any held inbound ticket
(`LXMessage.py:271-277`). An implementation MUST use `truncated_hash` (16 bytes),
matching the stamp width expectation of this path.

## Issuing

`generate_ticket(destination_hash, expiry)` (`LXMRouter.py:1025-1052`) returns
`[expires, ticket]` where:

- `ticket = os.urandom(16)` (`LXMRouter.py:1048`);
- `expires = now + TICKET_EXPIRY` (`LXMRouter.py:1047`).

An existing inbound ticket with more than `TICKET_RENEW` validity left is reused
rather than reissued (`LXMRouter.py:1083-1089`), and a new ticket is not issued to
a destination more often than `TICKET_INTERVAL` (`LXMRouter.py:1076-1081`).

## Exchange

A ticket is delivered to a correspondent inside a message via `FIELD_TICKET`
(0x0C, `LXMF.py:19`), carrying the `[expires, ticket]` pair. The receiver remembers
it as an outbound ticket (`remember_ticket`, `LXMRouter.py:1102-1105`) and uses it
for subsequent stamps until it expires (`get_outbound_ticket`,
`LXMRouter.py:1107-1113`).

The Rust router follows the same default: `enqueue()` automatically derives and
attaches the 16-byte delivery stamp whenever `TicketStore` holds a valid
outbound ticket for the destination. This happens before propagated recipient
encryption, but the ticket stamp remains distinct from the required 32-byte
outer propagation-node stamp. To grant a reply ticket,
`issue_ticket_field()` returns the bounded/persisted `FIELD_TICKET` value that
must be passed to `Message::create()` so it is covered by the signature.

## Timing constants

| Constant | Value | Seconds | Citation |
|----------|-------|---------|----------|
| `TICKET_EXPIRY` | 21 days | 1 814 400 | `LXMessage.py:48` |
| `TICKET_GRACE` | 5 days | 432 000 | `LXMessage.py:49` |
| `TICKET_RENEW` | 14 days | 1 209 600 | `LXMessage.py:50` |
| `TICKET_INTERVAL` | 1 day | 86 400 | `LXMessage.py:51` |

The validity windows are part of the interoperable behaviour: a ticket can
stamp messages until its encoded expiry, while the issuer retains its record
for the additional `TICKET_GRACE` cleanup window. The exact reuse and reissue
scheduling around them is informative.
