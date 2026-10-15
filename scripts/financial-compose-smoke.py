#!/usr/bin/env python3
"""Rehearse Compose funding, alerts, PostgreSQL outage and a coordinated backup/restore."""

import argparse
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def wait(predicate, timeout=90):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, RuntimeError, http.client.HTTPException):
            pass
        time.sleep(0.2)
    raise RuntimeError("operational rehearsal timed out")


def run(output):
    os.umask(0o077)
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    live = output / "live"
    subprocess.run(
        [
            "python3",
            str(ROOT / "scripts/financial-configure.py"),
            "--output",
            str(live),
        ],
        check=True,
    )
    compose = [
        "docker",
        "compose",
        "--env-file",
        str(live / "compose.env"),
        "-f",
        str(ROOT / "ops/financial/compose.yml"),
    ]
    commands = compose[:]
    checks = []

    def run_compose(*args, **kw):
        return subprocess.run([*commands, *args], check=True, **kw)

    def api(method, path, payload=None, caller="admin", expected=200):
        ctx = ssl.create_default_context(cafile=str(live / "ca.pem"))
        ctx.load_cert_chain(
            str(live / (caller + ".pem")), str(live / (caller + ".key"))
        )
        client = http.client.HTTPSConnection("localhost", 9443, context=ctx, timeout=3)
        try:
            client.request(
                method,
                path,
                None if payload is None else json.dumps(payload),
                {"Content-Type": "application/json"},
            )
            response = client.getresponse()
            body = response.read()
            if response.status != expected:
                raise RuntimeError(f"{path}: HTTP {response.status}: {body.decode()}")
            return json.loads(body) if body else None
        finally:
            client.close()

    try:
        run_compose("up", "-d", stdout=subprocess.DEVNULL)
        wait(lambda: api("GET", "/health/ready")["ready"])
        account = api(
            "POST", "/v1/admin/accounts", {"key": "operator", "name": "restore-test"}
        )["account_id"]
        api(
            "POST",
            f"/v1/accounts/{account}/funding",
            dict(asset="USDT", amount="1000", external_reference="manual-test"),
        )
        seller = api("POST", "/v1/admin/accounts", {"key": "seller", "name": "seller"})[
            "account_id"
        ]
        api(
            "POST",
            f"/v1/accounts/{seller}/funding",
            dict(asset="BTC", amount="10", external_reference="seller-inventory"),
        )

        def place(account, key, side, **extra):
            order = dict(
                symbol=0,
                side=side,
                order_type="limit",
                time_in_force="GTC",
                price="100",
                quantity="1",
            )
            order.update(extra)
            receipt = api(
                "POST",
                f"/v1/accounts/{account}/orders",
                {"key": key, "order": order},
                caller="trader",
                expected=202,
            )
            wait(
                lambda: (
                    api(
                        "GET", f"/v1/accounts/{account}/requests/{key}", caller="trader"
                    )["status"]
                    != "pending"
                )
            )
            return receipt["order_id"]

        buy = place(account, "buy", "buy")
        place(seller, "sell", "sell")
        wait(
            lambda: (
                api("GET", f"/v1/accounts/{account}/orders/{buy}", caller="trader")[
                    "status"
                ]
                == "filled"
            )
        )
        iceberg = place(
            account, "iceberg", "buy", price="90", quantity="2", display_quantity="1"
        )
        stop = place(
            account,
            "stop",
            "buy",
            order_type="stop_market",
            stop_price="110",
            protection_price="120",
        )
        expected = api("GET", f"/v1/accounts/{account}/balances")
        assert expected == [
            {"asset": "BTC", "available": "1", "reserved": "0"},
            {"asset": "USDT", "available": "600", "reserved": "300"},
        ], expected
        checks.append("mTLS readiness, account provisioning and privileged funding")
        request = urllib.request.Request(
            "http://127.0.0.1:9093/api/v2/alerts",
            data=json.dumps(
                [{"labels": {"alertname": "PilotDeliveryTest", "severity": "warning"}}]
            ).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=3) as response:
            assert response.status == 200

        def delivered():
            log = subprocess.check_output(
                [*commands, "logs", "alert-receiver"], stderr=subprocess.DEVNULL
            ).decode()
            return "PilotDeliveryTest" in log

        wait(delivered, 30)
        checks.append("Alertmanager notification delivered to the local receiver")
        run_compose("stop", "postgres", stdout=subprocess.DEVNULL)
        # Operations cannot acquire the database lease while PostgreSQL is unavailable.
        try:
            api(
                "POST",
                f"/v1/accounts/{account}/funding",
                dict(asset="USDT", amount="5", external_reference="must-not-commit"),
            )
        except (OSError, RuntimeError, http.client.HTTPException):
            pass
        else:
            raise AssertionError("funding succeeded while PostgreSQL was unavailable")
        run_compose("start", "postgres", stdout=subprocess.DEVNULL)
        run_compose("up", "-d", "gateway", stdout=subprocess.DEVNULL)
        wait(lambda: api("GET", "/health/ready")["ready"])
        assert api("GET", f"/v1/accounts/{account}/balances") == expected
        checks.append(
            "PostgreSQL outage stops admission without changing funded balances"
        )
        archive = output / "backup"
        subprocess.run(
            [
                "python3",
                str(ROOT / "scripts/financial-backup.py"),
                "--config-dir",
                str(live),
                "--output",
                str(archive),
            ],
            check=True,
        )
        before = api("GET", "/v1/admin/checkpoint")
        container = (
            subprocess.check_output([*commands, "ps", "-q", "postgres"])
            .decode()
            .strip()
        )
        run_compose(
            "exec",
            "-T",
            "postgres",
            "createdb",
            "-U",
            "matching",
            "restored_pilot",
            stdout=subprocess.DEVNULL,
        )
        env = dict(
            os.environ,
            ME_RESTORE_DATABASE_URL="postgres://matching@localhost/restored_pilot",
        )
        restored = output / "restored"
        subprocess.run(
            [
                "python3",
                str(ROOT / "scripts/financial-restore.py"),
                "--archive",
                str(archive),
                "--output",
                str(restored),
                "--postgres-container",
                container,
            ],
            env=env,
            check=True,
        )
        # A second restore into this nonempty database is forbidden, even with a new directory.
        denied = subprocess.run(
            [
                "python3",
                str(ROOT / "scripts/financial-restore.py"),
                "--archive",
                str(archive),
                "--output",
                str(output / "forbidden"),
                "--postgres-container",
                container,
            ],
            env=env,
            capture_output=True,
            text=True,
        )
        assert (
            denied.returncode != 0
            and "new empty database" in denied.stderr + denied.stdout
        )
        values = dict(
            line.split("=", 1)
            for line in (live / "compose.env").read_text().splitlines()
        )
        override = {
            "services": {
                "matcher": {
                    "volumes": [f"{restored}:/pilot:ro", f"{restored}/engine:/data"]
                },
                "gateway": {
                    "volumes": [f"{restored}:/pilot:ro"],
                    "environment": {
                        "ME_LEDGER_DATABASE_URL": f"postgres://matching:{values['POSTGRES_PASSWORD']}@postgres:5432/restored_pilot"
                    },
                },
            }
        }
        override_path = output / "restore-compose.json"
        override_path.write_text(json.dumps(override))
        override_path.chmod(0o600)
        run_compose("stop", "gateway", "matcher", stdout=subprocess.DEVNULL)
        commands = [*compose, "-f", str(override_path)]
        run_compose("up", "-d", "matcher", "gateway", stdout=subprocess.DEVNULL)
        restored_ready = wait(
            lambda: api("GET", "/health/ready", expected=503)["ledger"]["maintenance"]
        )
        after = api("GET", "/v1/admin/checkpoint")
        assert (
            before["watermark"] == after["watermark"]
            and before["identity"]["engine_id"] == after["identity"]["engine_id"]
        )
        assert api("GET", f"/v1/accounts/{account}/balances") == expected
        api("POST", "/v1/admin/reconcile")
        api("POST", "/v1/admin/maintenance", {"enabled": False})
        wait(lambda: api("GET", "/health/ready")["ready"])
        checks.append(
            "coordinated backup restored into a new database and directory; cursor, identity and balances agree; overwrite refused"
        )
        report = dict(status="passed", checks=checks, checkpoint=after["watermark"])
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))
    finally:
        run_compose("stop", stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    run(Path(parser.parse_args().output).resolve())
