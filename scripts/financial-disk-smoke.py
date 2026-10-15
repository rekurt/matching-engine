#!/usr/bin/env python3
"""Fill an isolated 8 MiB tmpfs journal volume, then recover a reserved intention."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import uuid

_spec = importlib.util.spec_from_file_location(
    "financial_smoke", Path(__file__).with_name("financial-smoke.py")
)
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
Pilot, wait = _module.Pilot, _module.wait


def run(args):
    pilot = Pilot(
        Path(args.output).resolve(),
        Path(args.bin_dir).resolve(),
        os.environ["LEDGER_TEST_DATABASE_URL"],
    )
    name = "me-disk-" + uuid.uuid4().hex[:12]
    volume = name + "-volume"
    holder = name + "-holder"
    matcher = name + "-matcher"

    def docker(*args, **kw):
        return subprocess.run(["docker", *args], check=True, **kw)

    try:
        docker(
            "volume",
            "create",
            "--driver",
            "local",
            "--opt",
            "type=tmpfs",
            "--opt",
            "device=tmpfs",
            "--opt",
            "o=size=8m,mode=0777",
            volume,
            stdout=subprocess.DEVNULL,
        )
        docker(
            "run",
            "-d",
            "--name",
            holder,
            "--mount",
            f"source={volume},target=/bounded",
            "alpine:3.22",
            "sleep",
            "7200",
            stdout=subprocess.DEVNULL,
        )
        pilot.engine_config.update(
            listen="0.0.0.0:9090", data_directory="/bounded", minimum_free_bytes=0
        )
        pilot.write_config()
        docker(
            "run",
            "-d",
            "--name",
            matcher,
            "--user",
            f"{os.getuid()}:{os.getgid()}",
            "-p",
            f"127.0.0.1:{pilot.engine_port}:9090",
            "--mount",
            f"source={volume},target=/bounded",
            "-v",
            f"{pilot.root}:{pilot.root}:ro",
            args.image,
            "--financial-config",
            str(pilot.root / "matcher.json"),
            stdout=subprocess.DEVNULL,
        )
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
        account = pilot.api(
            "POST", "/v1/admin/accounts", {"key": "buyer", "name": "buyer"}
        )["account_id"]
        pilot.api(
            "POST",
            f"/v1/accounts/{account}/funding",
            dict(asset="USDT", amount="1000", external_reference="initial"),
        )
        full = subprocess.run(
            [
                "docker",
                "exec",
                holder,
                "dd",
                "if=/dev/zero",
                "of=/bounded/filler",
                "bs=4096",
            ],
            capture_output=True,
            text=True,
        )
        assert full.returncode != 0 and "No space left on device" in full.stderr, (
            full.stderr
        )
        request = {
            "key": "disk-order",
            "order": dict(
                symbol=0,
                side="buy",
                order_type="limit",
                time_in_force="GTC",
                price="100",
                quantity="0.01",
            ),
        }
        receipts = []
        for index in range(20):
            item = dict(request, key=f"disk-order-{index}")
            try:
                receipt = pilot.api(
                    "POST",
                    f"/v1/accounts/{account}/orders",
                    item,
                    caller="trader",
                    expected=202,
                )
            except AssertionError as e:
                if "financial admission is paused" not in str(e):
                    raise
                break
            receipts.append((item, receipt))

        def paused():
            try:
                return pilot.api("GET", "/health/ready", expected=503)["ledger"]
            except AssertionError:
                return None

        state = wait(paused)
        assert state["reconciliation_reason"] is None, state
        assert state["pending_outbox"] >= 1, state
        assert all(
            pilot.order(account, r["order_id"])["reserved"] == "1.002"
            for _, r in receipts
        )
        docker("exec", holder, "rm", "/bounded/filler")
        subprocess.run(
            ["docker", "kill", matcher],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        docker("start", matcher, stdout=subprocess.DEVNULL)
        wait(pilot.ready, 60)
        for request, receipt in receipts:
            wait(lambda: pilot.order(account, receipt["order_id"])["status"] == "open")
            assert (
                pilot.api(
                    "POST",
                    f"/v1/accounts/{account}/orders",
                    request,
                    caller="trader",
                    expected=202,
                )
                == receipt
            )
            pilot.api(
                "POST",
                f"/v1/accounts/{account}/orders/{receipt['order_id']}/cancel",
                {"key": "cancel-" + request["key"]},
                caller="trader",
                expected=202,
            )
            wait(
                lambda: (
                    pilot.order(account, receipt["order_id"])["status"] == "cancelled"
                )
            )
        assert pilot.api("GET", f"/v1/accounts/{account}/balances") == [
            {"asset": "BTC", "available": "0", "reserved": "0"},
            {"asset": "USDT", "available": "1000", "reserved": "0"},
        ]
        report = dict(
            status="passed",
            filesystem="isolated tmpfs",
            capacity_bytes=8 * 1024 * 1024,
            checks=[
                "real ENOSPC",
                "reserved intention retained without reconciliation latch",
                "restart truncates only incomplete tail and retries original identity",
                "confirmed cancellation releases reserve once",
            ],
        )
        (pilot.root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))
    finally:
        with (pilot.root / "matcher-container.log").open("wb") as log:
            subprocess.run(
                ["docker", "logs", matcher], stdout=log, stderr=subprocess.STDOUT
            )
        pilot.close()
        for container in [matcher, holder]:
            subprocess.run(
                ["docker", "rm", "-f", container],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        subprocess.run(
            ["docker", "volume", "rm", volume],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--bin-dir", default="target/debug")
    parser.add_argument("--image", default="matching-engine:financial-pilot")
    run(parser.parse_args())
