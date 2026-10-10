# HTTP API

The HTTP listener uses `server.metrics_addr`, default `127.0.0.1:9091`. It provides no native TLS. Protect it with a private network or authenticated proxy.

| Method | Path | Authentication | Result |
| --- | --- | --- | --- |
| GET | `/health` | none | `200 OK` or `503` when unavailable/draining |
| GET | `/ready` | none | Same availability check as health |
| GET | `/metrics` | none | Prometheus text exposition |
| GET | `/admin/status` | none | Service status and per-symbol trading state |
| POST | `/admin/halt/{symbol_id}` | `X-Admin-Key` | Durable halt |
| POST | `/admin/resume/{symbol_id}` | `X-Admin-Key` | Durable resume |

Halt/resume routes are not registered when `server.admin.api_key` is empty; requests then return 404. With a configured key, missing/incorrect credentials return 401. Keys require at least 16 printable, non-space ASCII characters. `/admin/status` remains public even when a key is configured.

```sh
curl --fail http://127.0.0.1:9091/ready
curl --fail http://127.0.0.1:9091/admin/status
curl --fail -X POST -H "X-Admin-Key: $ME_ADMIN_KEY" http://127.0.0.1:9091/admin/halt/0
```

`/health` and `/ready` return plain text: `OK` on success, or `draining` / `engine unavailable` with HTTP 503. They do not return JSON.

`GET /admin/status` example when the service is ready:

```json
{"status":"ready","symbols":[{"symbol_id":0,"trading_enabled":true}]}
```

Unavailable/draining `/admin/status` returns HTTP 503 with `{"status":"unavailable"}` or `{"status":"draining"}`. Health also checks trading listeners, session storage and shard workers. A deliberately halted symbol remains operationally healthy.

Halt/resume responses contain `symbol_id`, `action` and `success`. Success returns 200; an unknown symbol returns 404, and a failed engine operation returns 503. Existing orders remain in the book when a symbol is halted.

For order commands and events, use the [binary protocol](protocol.md).
