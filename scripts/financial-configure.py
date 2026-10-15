#!/usr/bin/env python3
"""Create a private, disposable local pilot configuration; never use its CA with real funds."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import ssl
import uuid


def configure(root):
    root = root.resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    if any(root.iterdir()):
        raise SystemExit(
            "configuration directory must be empty; existing identities are never replaced"
        )
    root.chmod(0o700)
    spec = importlib.util.spec_from_file_location(
        "financial_smoke", Path(__file__).with_name("financial-smoke.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    fixture = module.Pilot.__new__(module.Pilot)
    fixture.root = root
    fixture.certs()
    for name in ["matcher", "gateway"]:
        # These are the Compose DNS names as well as certificate subjects.
        import subprocess

        ext = root / (name + ".ext")
        ext.write_text(
            ext.read_text().replace("DNS:localhost,", f"DNS:{name},DNS:localhost,")
        )
        subprocess.run(
            [
                "openssl",
                "x509",
                "-req",
                "-days",
                "2",
                "-in",
                str(root / (name + ".csr")),
                "-CA",
                str(root / "ca.pem"),
                "-CAkey",
                str(root / "ca.key"),
                "-set_serial",
                "10" if name == "matcher" else "11",
                "-extfile",
                str(ext),
                "-out",
                str(root / (name + ".pem")),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def tls(name):
        return dict(
            certificate=f"/pilot/{name}.pem",
            private_key=f"/pilot/{name}.key",
            ca="/pilot/ca.pem",
        )

    def fingerprint(name):
        return hashlib.sha256(
            ssl.PEM_cert_to_DER_cert((root / (name + ".pem")).read_text())
        ).hexdigest()

    symbol = dict(
        id=0,
        base="BTC",
        quote="USDT",
        price_scale=2,
        qty_scale=8,
        min_qty=1000,
        max_qty=1000000000000,
        qty_step=1000,
        price_step=1000000,
        invariant_policy="strict",
    )
    matcher = dict(
        listen="0.0.0.0:9090",
        data_directory="/data",
        tls=tls("matcher"),
        gateway_fingerprints=[fingerprint("gateway")],
        symbols=[symbol],
    )
    gateway = dict(
        listen="0.0.0.0:9443",
        tls=tls("gateway"),
        matcher=dict(
            address="matcher:9090",
            server_name="matcher",
            certificate_fingerprint=fingerprint("matcher"),
            tls=tls("gateway"),
        ),
        principals={
            fingerprint("admin"): dict(
                all_accounts=True, permissions=["read", "admin", "funding"]
            ),
            fingerprint("trader"): dict(
                all_accounts=True, permissions=["read", "trade"]
            ),
            fingerprint("unauthorized"): dict(permissions=["read"]),
        },
        symbols=[symbol],
        fees={"0": dict(id=str(uuid.uuid4()), maker_bps=0, taker_bps=0)},
    )
    for name, config in [("matcher", matcher), ("gateway", gateway)]:
        (root / (name + ".json")).write_text(json.dumps(config, indent=2) + "\n")
    (root / "engine").mkdir(mode=0o700)
    password = secrets.token_hex(24)
    env = f"PILOT_PROJECT=matching-pilot-{hashlib.sha256(str(root).encode()).hexdigest()[:8]}\nPILOT_DIRECTORY={root}\nPILOT_UID={os.getuid()}\nPILOT_GID={os.getgid()}\nPOSTGRES_PASSWORD={password}\n"
    (root / "compose.env").write_text(env)
    (root / "compose.env").chmod(0o600)
    print(
        f"Created disposable configuration in {root}; certificates expire in two days."
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", default=".pilot")
    configure(Path(parser.parse_args().output))
