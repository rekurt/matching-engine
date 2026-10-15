#!/usr/bin/env python3
"""Restore a verified pilot archive into a NEW empty PostgreSQL database and directory."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import urllib.parse


def restore(archive, destination, container=None):
    if destination.exists():
        raise SystemExit(
            "restore destination must not exist; never overwrite a live deployment"
        )
    manifest = json.loads((archive / "manifest.json").read_text())
    if manifest["format"] != 1:
        raise SystemExit("unsupported backup format")
    for name, digest in manifest["files"].items():
        path = (archive / name).resolve()
        if (
            not path.is_relative_to(archive)
            or not path.is_file()
            or hashlib.sha256(path.read_bytes()).hexdigest() != digest
        ):
            raise SystemExit("backup file missing, invalid or changed: " + name)
    parts = urllib.parse.urlsplit(os.environ["ME_RESTORE_DATABASE_URL"])
    if parts.scheme not in ("postgres", "postgresql") or parts.query or parts.fragment:
        raise SystemExit(
            "restore URL must identify a new database without query options"
        )
    env = dict(
        os.environ,
        PGHOST=parts.hostname or "localhost",
        PGPORT=str(parts.port or 5432),
        PGUSER=urllib.parse.unquote(parts.username or "postgres"),
        PGPASSWORD=urllib.parse.unquote(parts.password or ""),
        PGDATABASE=parts.path.lstrip("/"),
    )
    # A fresh database is deliberately stricter than an empty financial schema.
    prefix = ["docker", "exec", "-i", container] if container else []
    count = (
        subprocess.check_output(
            prefix
            + [
                "psql",
                "-U",
                env["PGUSER"],
                "-d",
                env["PGDATABASE"],
                "-X",
                "-A",
                "-t",
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON c.relnamespace=n.oid WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND c.relkind IN ('r','v','m','S','f')",
            ],
            env=env,
        )
        .decode()
        .strip()
    )
    if count != "0":
        raise SystemExit(
            "restore requires a new empty database; overwriting confirmed trades is forbidden"
        )
    os.umask(0o077)
    with (archive / "ledger.dump").open("rb") as source:
        subprocess.run(
            prefix
            + [
                "pg_restore",
                "-U",
                env["PGUSER"],
                "--exit-on-error",
                "--single-transaction",
                "--no-owner",
                "--no-privileges",
                "-d",
                env["PGDATABASE"],
            ],
            env=env,
            stdin=source,
            check=True,
        )
    shutil.copytree(archive / "configuration", destination)
    shutil.copytree(archive / "engine", destination / "engine")
    destination.chmod(0o700)
    print(
        f"Restored paused files into {destination}. Configure the NEW database and paths, then verify /v1/admin/checkpoint before resuming. Never reuse the old Compose volume."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument(
        "--postgres-container",
        help="use psql and pg_restore inside a PostgreSQL container",
    )
    args = parser.parse_args()
    restore(
        Path(args.archive).resolve(),
        Path(args.output).resolve(),
        args.postgres_container,
    )
