# Leviculum public API design review, round 2

Input to the ffi-Agent, on top of `docs/api-design-review.md` (round 1) and
your own internal review folded into commit c11a916. Two independent reviewers
read the post-c11a916 design doc with their own contexts and verified claims
against the `leviculum-std` / `leviculum-core` source.

Both verdicts: ready to keep building. Round 1 plus your internal review are
confirmed resolved with technically correct citations (panic and poison
contract, the 64 byte key split, error mapping 1:1, returned identity
ownership, file IO as new facade surface, restart bridge created in `new` and
surviving stop/start, flat C enums, string conventions, logging, global init
and the clock anchor OnceLog first-writer-wins). The core carries.

This document lists only the residual items both reviewers found on top of
that. Nothing here is a redesign. The eventfd item is the one to settle before
phase b, because phase b builds the eventfd bridge.

## Settle before phase b (the eventfd bridge)

### R1. eventfd semaphore invariant has two precise seams (both reviewers, independently)
The semaphore-mode design (counter equals queue length) is correct and kills
both lost-wakeup and busy-loop. Two places can silently violate the invariant:

- Drop or coalesce path. If the bridge ever drops or evicts an already enqueued
  event (the full-data-region policy), it must perform a compensating `read(fd)`
  for the removed counted item, or enforce the cap at enqueue time so a dropped
  event was never counted and never wrote the fd. State which. Without it the
  counter exceeds the queue length and the fd is readable forever.
- Consumer read underflow. With `EFD_SEMAPHORE | EFD_NONBLOCK`, the consumer
  read can return EAGAIN in the window between the producer unlocking the FIFO
  and writing the fd. The consumer must tolerate EAGAIN (treat as "nothing to
  decrement right now", not a lost event). Specify that the consumer never
  treats EAGAIN as an error and never underflows.

State the producer order (lock, push, unlock, then write) and that the counter
equals queue length is an eventual invariant across that one window, not a
per-instant one. One paragraph closes it.

## Settle before the relevant phase calcifies the header

### R2. `lev_connect` identity resolution is new glue, not a thin projection (both reviewers)
The doc frames `lev_connect` as resolving the signing key internally and the
facade as adding no behaviour. The driver and core `connect` both require the
Ed25519 signing key passed in (bytes 32..64); neither does the lookup.
`Storage::get_identity(dest_hash) -> Option<&Identity>` exists, so it is
buildable and additive, but it is new lookup-and-slice code in the facade, not
re-typing. Label it as new surface like the file IO was, so phase c budgets for
it rather than expecting a one liner.

### R3. Hash and id returning functions are ABI ambiguous (round 2 fresh review)
Section 2 says fallible functions return `int`. Section 10 lists
`lev_send_datagram -> 16 byte hash`, `lev_send_request -> request id`,
`lev_send_resource -> 32 byte hash`. A function cannot return both `int` status
and a hash. Define them as status via `int`, value via an out parameter, e.g.
`int lev_send_datagram(node, dest, data, len, uint8_t out_hash[16])`, and state
it once as the convention (same as the event accessors already use).

### R4. Multi-payload events need per-field accessors (round 2 fresh review)
A single `lev_event_data` cannot express events with two payloads:
`ResourceCompleted` carries data plus metadata, `RequestReceived` carries a path
string plus data. Add per-field accessors (`lev_event_metadata`,
`lev_event_path`, `lev_event_request_id`) or make the v1 projection explicitly
flatten or drop the secondary field. Real surface gap for phases d and e, decide
before the header.

## Smaller, settle when convenient

- R5. Runtime label. Section 5 says "single-worker runtime"; the code builds
  `new_multi_thread().worker_threads(1)` on a separate thread. Relabel, and make
  explicit that the section 12 rule "a log callback must not call back into
  `lev_*`" is load-bearing for soundness (calling a blocking `lev_*` from a
  worker thread would `block_on` inside a runtime worker and panic), not just
  etiquette. Cross-reference it from section 5.
- R6. Post-timeout dispatch semantics. On `LEV_ERR_TIMEOUT` for the action
  enqueue calls, the action may or may not have been dispatched, and it is not
  auto-retried. The C caller cannot tell. State the contract.
- R7. `lev_init` lazy path must go through the same `Once` as explicit
  `lev_init`, so concurrent first calls do not race on subscriber and panic-hook
  installation.
- R8. Request handler allowlist entries are identity hashes (name them so, not
  generic ids), and there is no unregister in v1 (registration is
  additive/overwrite per dest and path). Say so or a C author looks for
  `lev_unregister_request_handler`.
- R9. Double `lev_start` on a running node maps to `LEV_ERR_CONFIG` (core
  returns `Error::Config("node already running")`), not a no-op.
- R10. `lev_send_datagram` needs a known path first (the sender errors with no
  path), same precondition as `lev_connect`. Map to `LEV_ERR_NO_PATH`.
- R11. Builder method name: doc says `lev_builder_add_udp`, facade method is
  `add_udp_interface`. Make the doc list match the actual `api::NodeBuilder`.
- R12. Prefer a wrapper or proc macro that generates the panic guard over a grep
  lint, so a function cannot be written without it. Keep the grep as a backstop.
- R13. `lev_version_number` is host byte order, for in-process compares only,
  not for serialization. Say so.

## Note for the merge (not the ffi-Agent)
The branch pins `panic = "unwind"` in the workspace release profile (root
Cargo.toml), which is behaviour identical to the default but touches the shared
file lnsd and lnstest build on. Rationale is correct (a cdylib with abort would kill
the host process on any internal panic). The only cost is losing an
abort-for-smaller-daemon-binaries option. Decide at merge time whether the
daemons want abort handled separately.
