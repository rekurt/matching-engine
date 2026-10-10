# Python example

`examples/python/client.py` demonstrates TCP framing with Python's standard library (`socket`, `struct` and decimal conversion). It requires Python 3.10 or newer.

```sh
python3 examples/python/client.py 127.0.0.1 9090
```

The example performs Handshake, submits orders on symbol 1 and reads events. It is intended for a fresh local test server. Its optional arguments are host and port; adapt its symbol and order values before using it with a different configuration.

Run its protocol tests with `python3 -m unittest discover -s examples/python`. The separate `scripts/runtime_smoke.py` test client exercises actual TCP/mTLS processes, crash recovery and session resume.

For production integration, implement persistent receive cursors, mutation retries and settlement reconciliation as described in [client integration](../integration.md).
