# On-air sequences

This section describes the on-air event sequence for each delivery method. The
**bytes** placed on the wire are normative (see
[Delivery methods and sizing](05-delivery-and-sizing.md)); the **scheduling**
(retry cadence, timeouts, path-request timing) is informative and lives in
[Router internals](11-router-informative.md). The relevant cadence constants are
`MAX_DELIVERY_ATTEMPTS = 5` (`LXMRouter.py:30`), `DELIVERY_RETRY_WAIT = 10 s`
(`LXMRouter.py:32`), `PATH_REQUEST_WAIT = 7 s` (`LXMRouter.py:33`), and
`MAX_PATHLESS_TRIES = 1` (`LXMRouter.py:34`).

## Opportunistic

1. If there is no path to the destination, request one and wait (informative
   cadence). After `MAX_PATHLESS_TRIES` the message may be sent pathless.
2. Send a single Reticulum Packet whose payload is `packed[16:]` (the destination
   hash is omitted; `LXMessage.py:631`).
3. The message state becomes `SENT`. Delivery is confirmed by a Reticulum proof;
   on timeout the router re-queues up to `MAX_DELIVERY_ATTEMPTS`.

No link is established. Suitable only for messages within the single-packet
content limit.

## Direct

1. Ensure a path, then establish a Reticulum `Link` to the destination's
   `lxmf/delivery` endpoint.
2. When the link is `ACTIVE` (`LXMessage.py:650`):
   - if representation is `PACKET`, send one Packet carrying the full `packed`
     bytes over the link (`LXMessage.py:633`);
   - if representation is `RESOURCE`, transfer `packed` as a Reticulum Resource
     over the link (`LXMessage.py:650-651`), with compression negotiated per the
     peer's advertised support.
3. On link failure before delivery, tear down and retry.

## Propagated

1. Establish a `Link` to the configured outbound propagation node.
2. Send `propagation_packed` (the encrypted envelope, see
   [Propagation](10-propagation.md)) as a Packet or Resource depending on size
   (`LXMessage.py:634-635,652-653`).
3. Success marks the message `SENT` (not `DELIVERED`): final delivery to the
   recipient happens asynchronously when the recipient syncs from the node.

## Paper

No Reticulum transport. `pack()` produces the encrypted paper form; `as_uri()`
renders it as an `lxm://` URI (`LXMessage.py:687-702`) or `as_qr()` as a QR code.
The recipient ingests the URI out of band.

## State model (informative)

A message moves through the states `GENERATING (0x00) -> OUTBOUND (0x01) ->
SENDING (0x02) -> SENT (0x04) -> DELIVERED (0x08)`, with terminal `REJECTED
(0xFD)`, `CANCELLED (0xFE)`, and `FAILED (0xFF)` (`LXMessage.py:14-21`). These
are local lifecycle states, not on-wire values, and an implementation MAY model
the lifecycle differently.
