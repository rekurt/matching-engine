#!/usr/bin/env python3
"""Back up a drained local Compose pilot; admission stays paused after completion."""

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import uuid


def request(root, method, path, payload=None):
    context = ssl.create_default_context(cafile=str(root / "ca.pem"))
    context.load_cert_chain(str(root / "admin.pem"), str(root / "admin.key"))
    client = http.client.HTTPSConnection("localhost", 9443, context=context, timeout=60)
    try:
        client.request(
            method,
            path,
            None if payload is None else json.dumps(payload),
            {"Content-Type": "application/json"},
        )
        response = client.getresponse()
        data = response.read()
        if response.status != 200:
            raise RuntimeError(f"{path}: HTTP {response.status}: {data.decode()}")
        return json.loads(data)
    finally:
        client.close()


def verify_orders(snapshot, rows):
    metadata = snapshot["orders"]
    books = {}
    for book in snapshot["books"]:
        for order in book["resting_orders"] + book["stop_orders"]:
            if order["id"] in books:
                raise RuntimeError("duplicate order in engine snapshot")
            books[order["id"]] = order
    if set(metadata) != set(str(row["id"]) for row in rows):
        raise RuntimeError("engine and ledger order identities differ")
    expected_open = set()
    for row in rows:
        order_id = row["id"]
        meta = metadata[str(order_id)]
        if any(
            meta[field] != row[field]
            for field in [
                "account_id",
                "version",
                "fee_schedule",
                "reservation_id",
                "spec",
                "status",
            ]
        ):
            raise RuntimeError(f"engine/ledger binding mismatch for order {order_id}")
        remaining = int(row["remaining"])
        held = int(row["held"])
        spec = row["spec"]
        if meta["status"] == "open":
            expected_open.add(order_id)
            book = books.get(order_id)
            if (
                book is None
                or book["qty"] - book["cumulative_qty"] != remaining
                or book["qty"] != spec["qty_raw"]
            ):
                raise RuntimeError(
                    f"engine/ledger remaining quantity mismatch for order {order_id}"
                )
            if (
                book["client_session_id"] != list(uuid.UUID(meta["account_id"]).bytes)
                or book["symbol"] != spec["symbol"]
                or book["side"].lower() != spec["side"]
            ):
                raise RuntimeError(
                    f"engine/ledger ownership mismatch for order {order_id}"
                )
        elif remaining != 0 or held != 0:
            raise RuntimeError(f"closed order still holds funds: {order_id}")
        price = (
            spec["protection_price_raw"]
            if spec["order_type"] in (1, 3)
            else spec["price_raw"]
        )
        required = (
            remaining * 10**12
            if spec["side"] == "sell"
            else price * remaining * (10000 + max(row["maker_bps"], row["taker_bps"]))
        )
        if held != required:
            raise RuntimeError(
                f"order reserve differs from the frozen fee model: {order_id}"
            )
    if set(books) != expected_open:
        raise RuntimeError("engine snapshot has unknown open orders")


def read_snapshot(path):
    envelope = json.loads(path.read_text())
    payload = envelope.get("payload")
    if not isinstance(payload, dict):
        raise RuntimeError("missing financial snapshot payload")
    encoded = json.dumps(
        payload,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode()
    if hashlib.sha256(encoded).hexdigest() != envelope.get("digest"):
        raise RuntimeError("financial snapshot checksum mismatch")
    return payload


def backup(root, output):
    if root == output or root in output.parents or output in root.parents:
        raise SystemExit("backup must be outside the live configuration directory")
    if output.exists():
        raise SystemExit("backup destination must not exist")
    os.umask(0o077)
    output.mkdir(parents=True, mode=0o700)
    request(root, "POST", "/v1/admin/maintenance", {"enabled": True})
    before = request(root, "GET", "/v1/admin/checkpoint")
    compose = Path(__file__).resolve().parents[1] / "ops/financial/compose.yml"
    command = [
        "docker",
        "compose",
        "--env-file",
        str(root / "compose.env"),
        "-f",
        str(compose),
    ]
    snapshot = read_snapshot(root / "engine/active/financial.snapshot.json")
    if (
        snapshot["watermark"] != before["watermark"]
        or snapshot["engine_id"] != before["identity"]["engine_id"]
    ):
        raise RuntimeError("snapshot is not at the agreed maintenance watermark")
    query = "SELECT COALESCE(json_agg(json_build_object('id',o.id,'account_id',o.account_id,'version',o.version,'fee_schedule',o.fee_schedule,'reservation_id',v.request_id,'spec',o.spec,'status',o.status,'remaining',o.remaining::TEXT,'held',o.held::TEXT,'maker_bps',f.maker_bps,'taker_bps',f.taker_bps)),'[]'::JSON) FROM orders o JOIN order_versions v ON v.order_id=o.id AND v.version=o.version JOIN fee_schedules f ON f.id=o.fee_schedule"
    rows = json.loads(
        subprocess.check_output(
            [
                *command,
                "exec",
                "-T",
                "postgres",
                "psql",
                "-X",
                "-A",
                "-t",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                "matching",
                "-d",
                "matching_pilot",
                "-c",
                query,
            ]
        )
    )
    verify_orders(snapshot, rows)
    with (output / "ledger.dump").open("wb") as destination:
        subprocess.run(
            [
                *command,
                "exec",
                "-T",
                "postgres",
                "pg_dump",
                "-U",
                "matching",
                "-d",
                "matching_pilot",
                "--format=custom",
            ],
            stdout=destination,
            check=True,
        )
    shutil.copytree(root / "engine", output / "engine")
    shutil.copytree(
        root, output / "configuration", ignore=shutil.ignore_patterns("engine")
    )
    after = request(root, "GET", "/v1/admin/checkpoint")
    if before != after:
        raise RuntimeError(
            "maintenance checkpoint changed during backup; archive is invalid"
        )
    files = {
        str(p.relative_to(output)): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in output.rglob("*")
        if p.is_file()
    }
    (output / "manifest.json").write_text(
        json.dumps(dict(format=1, checkpoint=before, files=files), indent=2) + "\n"
    )
    print(
        f"Backup verified at {output}; admission remains paused. Archive contains private keys and database credentials."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config-dir", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    backup(Path(args.config_dir).resolve(), Path(args.output).resolve())
