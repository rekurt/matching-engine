# Client integration

This page describes the legacy session protocol. Use the [internal financial gateway](financial-pilot.md) for funded account operations; its durable decision feed and PostgreSQL cursor provide settlement recovery independently of session replay.

Connect through TCP or UDS, then send Handshake or ResumeSession before order commands. The [wire specification](protocol.md) defines framing, payloads and sequence numbers.

Keep one ordered reader per connection. Correlate solicited events by `request_id`; passive fills and stop events use request ID zero. A command can produce several events. An order snapshot ends at OpenOrdersEnd, not at a period of socket silence. Handle fragmented reads and multiple frames in one read. Preserve passive events that arrive before or during a snapshot; commit the snapshot cursor only after validating Begin, End and the declared count.

Persist the session ID and the last applied sequence from the replay stream, as defined in the [protocol](protocol.md). Positive-sequence control frames also belong to replay; process and acknowledge them in order. Sequence-zero frames, including SessionResumed, never advance the cursor. Resume with the saved cursor, ignore duplicate replayed events, acknowledge only applied replay frames and retain the original request ID/payload when retrying a mutation. Session IDs are bearer credentials; use TLS and account authorization at the gateway.

If ReplayGap occurs, request GetOpenOrders and validate the complete Begin/Order/End response. That restores open orders, not missed settlement history; reconcile executions against an authoritative ledger before resuming financial processing. Batches are not atomic across symbols.

## Supplied clients

| Client | Use |
| --- | --- |
| [Go SDK](clients/go.md) | Existing TCP/UDS client and a separate recovering client with durable checkpoints |
| [Python example](clients/python.md) | Standard-library protocol demonstration |
| TypeScript example | Node.js Buffer framing and event parsing |
| Rust example | Tokio framing and basic order flow |

Examples live under `examples/` in the repository. They demonstrate framing and order flow. For durable session recovery, the Go SDK's separate `RecoveringClient` persists session identity, receive cursors and original pending mutation bytes. Its application callback must durably apply an event before acknowledgement; ReplayGap requires reconciliation. See the [Go guide](clients/go.md) for storage and delivery guarantees.

See [deployment](deployment.md) for ownership, durable retries, admission limits and storage requirements.
