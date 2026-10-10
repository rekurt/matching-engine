# Go client

The SDK is in `clients/go/meclient`; the runnable example is in `examples/go`.

```sh
cd examples/go
go run . 127.0.0.1:9090
```

`Dial` and `DialUDS` perform Handshake automatically. For TLS, establish a `tls.Conn`, pass it to `NewClientFromConn` and call `Handshake` before commands.

Use decimal strings for exact prices and quantities (`NewDecimalFromString`). Avoid floating-point conversion for financial input. ReadEvent returns individual events; ReadEvents provides streaming delivery.

The SDK serializes socket reads and writes, but Ping and Handshake consume their own replies. Do not run them concurrently with the event reader. Use one reading mode per connection.

The SDK supports basic submission/event operations for protocol v2. It does not provide a complete high-level ResumeSession/cursor/original-request-ID recovery API. A gateway must implement the [recovery contract](../integration.md) before relying on reconnects.
