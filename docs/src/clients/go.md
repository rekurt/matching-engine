# Go client

The SDK is in `clients/go/meclient`; the runnable example is in `examples/go`.

```sh
cd examples/go
go run . 127.0.0.1:9090
```

`Dial` and `DialUDS` perform Handshake automatically. For TLS, establish a `tls.Conn`, pass it to `NewClientFromConn` and call `Handshake` before commands.

Use decimal strings for exact prices and quantities (`NewDecimalFromString`). Avoid floating-point conversion for financial input. ReadEvent returns individual events; ReadEvents provides streaming delivery.

The SDK serializes socket reads and writes, but Ping and Handshake consume their own replies. Do not run them concurrently with the event reader. Use one reading mode per connection.

The original `Client` supports basic submission/event operations for protocol v2. `RecoveringClient` adds durable session state, ResumeSession, application checkpoints and retries with original IDs and bytes. Financial settlement still requires the [recovery contract](../integration.md) and an authoritative execution source.


## Recovering client

The existing `Client` remains available. `RecoveringClient` adds durable session identity,
request/client counters, application cursor and pending mutation bytes. Provide a `SessionStore`,
a context-aware dialer, a stable endpoint identifier, and an `ApplySession` callback. Run the
client in one goroutine and submit mutations from another.

`OpenFileSessionStore(path)` provides atomic rename + fsync checkpoints with mode `0600`
and an exclusive process lock on Linux, macOS and supported BSDs. Other platforms must supply
an appropriate store. Corrupt, missing existing or incompatible state fails closed. Never delete
the state file to work around a failed resume.

```go
store, err := meclient.OpenFileSessionStore("./private/session.json")
if err != nil { return err }
defer store.Close()
client, err := meclient.NewRecoveringClient(meclient.RecoveryOptions{
    Endpoint: "matching-service-tls-identity",
    Store: store,
    Dial: dialAuthenticatedConnection,
    Apply: applyToCheckpoint,
})
if err != nil { return err }
go func() { errors <- client.Run(ctx) }()
requestID, err := client.PlaceOrder(ctx, order)
```

The dialer is responsible for authenticating TLS and must not change service identity behind the
same endpoint label. The application callback receives positive-sequence control frames as well
as command replies and passive events. It returns the application's JSON checkpoint. The file
store saves that checkpoint with the cursor before the SDK sends an ACK. A database store must
commit application changes and the session checkpoint in the same transaction. Use an outbox for
external side effects. Do not synchronously call this client from its application callback.

Delivery is at least once across a crash before application commit. Correlate command replies by
`RecoveryFrame.RequestID`; passive events use zero. Commands are removed from pending state only
after an applied Pong barrier confirms preceding command processing. On reconnect the client
performs ResumeSession, applies replay and retransmits pending frames with their original IDs and
bytes. A submission timeout does not cancel an order; inspect the durable pending list before
submitting a new command.

ReplayGap persists `ReconciliationRequired` and stops recovery. It never starts a new session
silently or treats GetOpenOrders as recovery of lost trades. Reconcile executions and application
state before explicitly replacing or repairing a checkpoint. The recovering client speaks the
legacy session API; the funded gateway has its own persisted outbox and financial decision feed.

The bundled file store uses Unix advisory locks (Linux, macOS and supported BSDs).
On other platforms, provide a durable `SessionStore` with equivalent exclusive
ownership and atomic checkpoint guarantees. Do not remove or replace a live lock
file; detected replacement stops the store until reconciliation.
