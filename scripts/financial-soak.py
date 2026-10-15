#!/usr/bin/env python3
"""Measure the complete funded path with one driver and a midpoint kill/restart."""

import argparse
from decimal import Decimal, getcontext
import json
import math
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import hashlib
import time
import importlib.util

_spec = importlib.util.spec_from_file_location(
    "financial_smoke", Path(__file__).with_name("financial-smoke.py")
)
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
Pilot, wait = _module.Pilot, _module.wait


def percentile(values, fraction):
    return (
        sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]
        if values
        else None
    )


def run(args):
    getcontext().prec = 100
    root = Path(args.output).resolve()
    pilot = Pilot(
        root, Path(args.bin_dir).resolve(), os.environ["LEDGER_TEST_DATABASE_URL"]
    )
    samples = []
    completed = 0
    recovery = None
    errors = []
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
        for a, asset in [(buyer, "USDT"), (seller, "BTC")]:
            pilot.api(
                "POST",
                f"/v1/accounts/{a}/funding",
                dict(
                    asset=asset, amount="1000000000", external_reference="soak-" + asset
                ),
            )
        started = time.monotonic()
        wal = root / "engine" / "active" / "financial.wal"
        initial = wal.stat().st_size

        def database_bytes():
            return int(
                subprocess.check_output(
                    [
                        "psql",
                        "-X",
                        "-A",
                        "-t",
                        "-c",
                        "SELECT COALESCE(sum(pg_total_relation_size(c.oid)),0)::BIGINT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='"
                        + pilot.schema
                        + "' AND c.relkind IN ('r','m')",
                    ],
                    env=pilot.pg_env,
                )
                .decode()
                .strip()
            )

        initial_database_bytes = database_bytes()
        while time.monotonic() - started < args.seconds:
            begin = time.monotonic()
            key = f"load-{completed}"
            buy = {
                "key": key,
                "order": dict(
                    symbol=0,
                    side="buy",
                    order_type="limit",
                    time_in_force="GTC",
                    price="100",
                    quantity="0.00001",
                ),
            }
            sell = {
                "key": key,
                "order": dict(
                    symbol=0,
                    side="sell",
                    order_type="limit",
                    time_in_force="GTC",
                    price="100",
                    quantity="0.00001",
                ),
            }
            b = pilot.api(
                "POST",
                f"/v1/accounts/{buyer}/orders",
                buy,
                caller="trader",
                expected=202,
            )
            s = pilot.api(
                "POST",
                f"/v1/accounts/{seller}/orders",
                sell,
                caller="trader",
                expected=202,
            )
            if recovery is None and time.monotonic() - started >= args.seconds / 2:
                fault = time.monotonic()
                pilot.kill("gateway")
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
                wait(pilot.ready, 120)
                assert (
                    pilot.api(
                        "POST",
                        f"/v1/accounts/{buyer}/orders",
                        buy,
                        caller="trader",
                        expected=202,
                    )
                    == b
                )
                assert (
                    pilot.api(
                        "POST",
                        f"/v1/accounts/{seller}/orders",
                        sell,
                        caller="trader",
                        expected=202,
                    )
                    == s
                )
                recovery = time.monotonic() - fault
            wait(lambda: pilot.order(seller, s["order_id"])["status"] == "filled", 60)
            assert pilot.order(buyer, b["order_id"])["status"] == "filled"
            completed += 1
            samples.append((time.monotonic() - begin) * 1000)
            if completed % 100 == 0:
                (root / "progress.json").write_text(
                    json.dumps(
                        dict(
                            elapsed_seconds=time.monotonic() - started,
                            completed_trades=completed,
                            journal_bytes=wal.stat().st_size,
                        )
                    )
                )
        elapsed = time.monotonic() - started
        buyer_bal = {
            b["asset"]: b for b in pilot.api("GET", f"/v1/accounts/{buyer}/balances")
        }
        seller_bal = {
            b["asset"]: b for b in pilot.api("GET", f"/v1/accounts/{seller}/balances")
        }
        assert Decimal(buyer_bal["USDT"]["available"]) == Decimal(
            "1000000000"
        ) - completed * Decimal("0.001001")
        assert Decimal(buyer_bal["BTC"]["available"]) == completed * Decimal("0.00001")
        assert Decimal(seller_bal["USDT"]["available"]) == completed * Decimal(
            "0.000998"
        )
        assert Decimal(seller_bal["BTC"]["available"]) == Decimal(
            "1000000000"
        ) - completed * Decimal("0.00001")
        assert all(
            b["reserved"] == "0"
            for b in list(buyer_bal.values()) + list(seller_bal.values())
        )
        barrier = pilot.api("POST", "/v1/admin/maintenance", {"enabled": True})
        pilot.api("POST", "/v1/admin/reconcile")
        report = dict(
            status="passed",
            duration_seconds=elapsed,
            requested_seconds=args.seconds,
            confirmed_trades=completed,
            confirmed_commands=completed * 2,
            commands_per_second=completed * 2 / elapsed,
            latency_ms=dict(
                p50=percentile(samples, 0.5),
                p95=percentile(samples, 0.95),
                p99=percentile(samples, 0.99),
            ),
            recovery_seconds=recovery,
            errors=errors,
            journal_growth_bytes=wal.stat().st_size - initial,
            database_growth_bytes=database_bytes() - initial_database_bytes,
            binaries={
                name: hashlib.sha256((pilot.bin / name).read_bytes()).hexdigest()
                for name in ["me-server", "me-gateway"]
            },
            workload=dict(
                drivers=1,
                symbol="BTC/USDT",
                quantity="0.00001",
                price="100",
                maker_bps=10,
                taker_bps=20,
                description="Sequential HTTP placement pair, settlement polling with fresh TLS per HTTP request and balance verification; latency covers a complete placement pair, not engine-only matching",
            ),
            machine=dict(
                platform=platform.platform(),
                architecture=platform.machine(),
                logical_cpus=os.cpu_count(),
                free_disk_bytes=shutil.disk_usage(root).free,
            ),
            barrier=barrier,
            schema=pilot.schema,
        )
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))
    finally:
        pilot.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=3600)
    parser.add_argument("--bin-dir", default="target/release")
    parser.add_argument("--output", required=True)
    arguments = parser.parse_args()
    if arguments.seconds < 1:
        parser.error("--seconds must be positive")
    run(arguments)
