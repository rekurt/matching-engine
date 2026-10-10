# meclient

Go SDK for matching-engine protocol v2. Connect using `Dial` (TCP) or `DialUDS`; both perform Handshake. Use `NewDecimalFromString` for exact financial values.

See the [Go guide](../../../docs/src/clients/go.md), [wire specification](../../../docs/src/protocol.md) and runnable example in `examples/go`.

Run `go test -race ./...` from this directory. The SDK exposes submission/event APIs but does not implement complete session-resume or settlement recovery; see the [integration contract](../../../docs/src/integration.md).
