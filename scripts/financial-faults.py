#!/usr/bin/env python3
"""Deterministic WAL/result-loss, duplicate dispatch and rollback/corruption fault rehearsal."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import socket
import shutil
import ssl
import struct
import subprocess
import time

_spec = importlib.util.spec_from_file_location(
    "financial_smoke", Path(__file__).with_name("financial-smoke.py")
)
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
Pilot, wait = _module.Pilot, _module.wait


def sql(pilot, query):
    return (
        subprocess.check_output(
            ["psql", "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", query],
            env=pilot.pg_env,
        )
        .decode()
        .strip()
    )


class Wire:
    def __init__(self, pilot):
        context = ssl.create_default_context(cafile=str(pilot.root / "ca.pem"))
        context.load_cert_chain(
            str(pilot.root / "gateway.pem"), str(pilot.root / "gateway.key")
        )
        self.sock = context.wrap_socket(
            socket.create_connection(("127.0.0.1", pilot.engine_port), timeout=3),
            server_hostname="localhost",
        )
        self.sock.settimeout(3)
        capabilities = (1 << 5) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11)
        self.sock.sendall(
            self.frame(5, 0, 0, b"\x02" + struct.pack("<HHQ", 1, 8, capabilities))
        )
        assert self.read()[0] == 0x1A

    @staticmethod
    def frame(kind, sequence, request, payload):
        return struct.pack("<IBBQQ", len(payload), 2, kind, sequence, request) + payload

    def read(self):
        def exact(n):
            data = b""
            while len(data) < n:
                block = self.sock.recv(n - len(data))
                if not block:
                    raise EOFError("financial connection closed")
                data += block
            return data

        length, version, kind, sequence, request = struct.unpack("<IBBQQ", exact(22))
        assert version == 2
        return kind, sequence, request, exact(length)

    def close(self):
        self.sock.close()


def run(args):
    pilot = Pilot(
        Path(args.output).resolve(),
        Path(args.bin_dir).resolve(),
        os.environ["LEDGER_TEST_DATABASE_URL"],
    )
    checks = []
    try:
        pilot.start("matcher")
        wait(
            lambda: (
                socket.create_connection(
                    ("127.0.0.1", pilot.engine_port), timeout=0.2
                ).close()
                is None
            )
        )
        pilot.start("gateway")
        wait(pilot.ready, 60)
        buyer = pilot.api(
            "POST", "/v1/admin/accounts", {"key": "buyer", "name": "buyer"}
        )["account_id"]
        seller = pilot.api(
            "POST", "/v1/admin/accounts", {"key": "seller", "name": "seller"}
        )["account_id"]
        for account, asset, amount in [(buyer, "USDT", "1000"), (seller, "BTC", "10")]:
            pilot.api(
                "POST",
                f"/v1/accounts/{account}/funding",
                dict(asset=asset, amount=amount, external_reference=asset),
            )

        def place(account, side, key):
            return pilot.api(
                "POST",
                f"/v1/accounts/{account}/orders",
                {
                    "key": key,
                    "order": dict(
                        symbol=0,
                        side=side,
                        order_type="limit",
                        time_in_force="GTC",
                        price="100",
                        quantity="1",
                    ),
                },
                caller="trader",
                expected=202,
            )

        b = place(buyer, "buy", "buy")
        wait(lambda: pilot.order(buyer, b["order_id"])["status"] == "open")
        early = pilot.root / "early-engine"
        shutil.copytree(pilot.root / "engine/active", early)
        before = sql(pilot, f"SELECT sequence FROM {pilot.schema}.cursors")
        pilot.proxy.pause()
        s = place(seller, "sell", "sell")
        assert (
            sql(
                pilot, f"SELECT count(*) FROM {pilot.schema}.outbox WHERE NOT completed"
            )
            == "1"
        )
        pilot.kill("gateway")
        pilot.proxy.resume()
        row = json.loads(
            sql(
                pilot,
                f"SELECT json_build_object('request_id',request_id,'wire',engine_request_id,'payload',payload,'encoded',encode(encoded_frame,'hex')) FROM {pilot.schema}.outbox WHERE NOT completed",
            )
        )
        row["payload"]["request_id"] = row["request_id"]
        frame = Wire.frame(
            0x30,
            row["wire"],
            row["wire"],
            json.dumps(row["payload"], separators=(",", ":")).encode(),
        )
        if row["encoded"]:
            frame = bytes.fromhex(row["encoded"])
        else:
            sql(
                pilot,
                f"UPDATE {pilot.schema}.outbox SET encoded_frame=decode('{frame.hex()}','hex') WHERE request_id='{row['request_id']}'",
            )
        wire = wait(lambda: Wire(pilot), 5)
        wire.sock.sendall(frame)
        reply = json.loads(wire.read()[3])
        assert reply["error"] is None
        decision = reply["result"]
        assert len(decision["fills"]) == 1
        assert sql(pilot, f"SELECT sequence FROM {pilot.schema}.cursors") == before
        # The fixture stands in for a dispatcher that dies before delivering the WAL acknowledgement to settlement.
        wire.sock.sendall(frame)
        assert json.loads(wire.read()[3])["result"] == decision
        wire.close()
        checks.append(
            "WAL committed with PostgreSQL cursor unchanged; duplicate command returns the same decision"
        )
        pilot.kill("matcher")
        pilot.start("matcher")
        wait(
            lambda: (
                socket.create_connection(
                    ("127.0.0.1", pilot.engine_port), timeout=0.2
                ).close()
                is None
            )
        )
        pilot.start("gateway")
        wait(pilot.ready, 60)
        wait(lambda: pilot.order(seller, s["order_id"])["status"] == "filled")
        assert (
            sql(
                pilot,
                f"SELECT sum(jsonb_array_length(payload->'fills')) FROM {pilot.schema}.decisions",
            )
            == "1"
        )
        assert pilot.api("GET", f"/v1/accounts/{buyer}/balances") == [
            {"asset": "BTC", "available": "1", "reserved": "0"},
            {"asset": "USDT", "available": "899.9", "reserved": "0"},
        ]
        checks.append(
            "hard restart replays exactly one trade and both fees; persisted original outbox bytes replay safely"
        )
        # A gateway restart after settlement must retain the cursor and completed outbox.
        cursor = sql(pilot, f"SELECT sequence FROM {pilot.schema}.cursors")
        pilot.kill("gateway")
        pilot.start("gateway")
        wait(pilot.ready, 60)
        assert sql(pilot, f"SELECT sequence FROM {pilot.schema}.cursors") == cursor
        assert (
            sql(
                pilot, f"SELECT count(*) FROM {pilot.schema}.outbox WHERE NOT completed"
            )
            == "0"
        )
        checks.append("restart after settlement does not debit again")
        pilot.proxy.pause()
        pending = place(buyer, "buy", "low-disk-pending")
        assert (
            sql(
                pilot, f"SELECT count(*) FROM {pilot.schema}.outbox WHERE NOT completed"
            )
            == "1"
        )
        pilot.kill("gateway")
        pilot.kill("matcher")
        pilot.proxy.resume()
        pilot.engine_config["minimum_free_bytes"] = 2**63
        pilot.write_config()
        pilot.start("matcher")
        wait(
            lambda: (
                socket.create_connection(
                    ("127.0.0.1", pilot.engine_port), timeout=0.2
                ).close()
                is None
            )
        )
        wire = wait(lambda: Wire(pilot), 5)
        wire.sock.sendall(frame)
        denied = json.loads(wire.read()[3])
        wire.close()
        assert denied["retryable"] and denied["result"] is None, denied
        pilot.start("gateway")

        def paused_without_latch():
            state = pilot.api("GET", "/health/ready", expected=503)
            return (
                state["ledger"]["reconciliation_reason"] is None
                and state["ledger"]["pending_outbox"] == 1
            )

        wait(paused_without_latch, 30)
        assert pilot.order(buyer, pending["order_id"])["reserved"] == "100.2"
        assert place(buyer, "buy", "low-disk-pending") == pending
        pilot.kill("gateway")
        pilot.kill("matcher")
        pilot.engine_config.pop("minimum_free_bytes")
        pilot.write_config()
        pilot.start("matcher")
        wait(
            lambda: (
                socket.create_connection(
                    ("127.0.0.1", pilot.engine_port), timeout=0.2
                ).close()
                is None
            )
        )
        pilot.start("gateway")
        wait(pilot.ready, 60)
        wait(lambda: pilot.order(buyer, pending["order_id"])["status"] == "open")
        pilot.api(
            "POST",
            f"/v1/accounts/{buyer}/orders/{pending['order_id']}/cancel",
            {"key": "finish-low-disk"},
            caller="trader",
            expected=202,
        )
        wait(lambda: pilot.order(buyer, pending["order_id"])["status"] == "cancelled")
        checks.append(
            "disk reserve pauses admission without latching reconciliation; queued reserve and stable receipt survive until recovery"
        )
        pilot.api("POST", "/v1/admin/maintenance", {"enabled": True})
        checkpoint = pilot.api("GET", "/v1/admin/checkpoint")
        checks.append("drained maintenance checkpoint agrees with ledger cursor")
        # A complete corrupt record must stop startup, not be discarded as an incomplete tail.
        pilot.kill("gateway")
        pilot.kill("matcher")
        journal = pilot.root / "engine/active/financial.wal"
        original = journal.read_bytes()
        bad = bytearray(original)
        bad[-1] ^= 1
        journal.write_bytes(bad)
        pilot.start("matcher")
        process = pilot.processes["matcher"]
        assert process.wait(timeout=10) != 0
        journal.write_bytes(original)
        checks.append("complete WAL corruption fails closed")
        good = pilot.root / "good-engine"
        shutil.copytree(pilot.root / "engine/active", good)
        shutil.rmtree(pilot.root / "engine/active")
        shutil.copytree(early, pilot.root / "engine/active")
        pilot.start("matcher")
        wait(
            lambda: (
                socket.create_connection(
                    ("127.0.0.1", pilot.engine_port), timeout=0.2
                ).close()
                is None
            )
        )
        pilot.start("gateway")
        wait(
            lambda: pilot.api("GET", "/health/ready", expected=503)["ledger"][
                "reconciliation_reason"
            ],
            30,
        )
        pilot.api("POST", "/v1/admin/maintenance", {"enabled": True})
        pilot.api("POST", "/v1/admin/reconcile", expected=409)
        checks.append(
            "old engine backup against a newer ledger latches reconciliation and cannot clear the cursor mismatch"
        )
        report = dict(status="passed", checks=checks, checkpoint=checkpoint)
        (pilot.root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(dict(status="passed", checks=len(checks))))
    finally:
        pilot.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--bin-dir", default="target/debug")
    run(parser.parse_args())
