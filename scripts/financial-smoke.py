#!/usr/bin/env python3
"""Isolated funded path: PostgreSQL, mTLS, accounting, retries and real process restarts."""

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import threading
import urllib.parse
import uuid


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait(predicate, timeout=30):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, ValueError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError("timed out waiting for financial service state")


class TLSProxy:
    def __init__(self, backend):
        self.backend = backend
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self.condition = threading.Condition()
        self.paused = False
        self.closed = False
        self.connections = []
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while not self.closed:
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            try:
                server = socket.create_connection(
                    ("127.0.0.1", self.backend), timeout=5
                )
                server.settimeout(None)
            except OSError:
                client.close()
                continue
            self.connections.extend([client, server])
            threading.Thread(
                target=self.pump, args=(client, server, True), daemon=True
            ).start()
            threading.Thread(
                target=self.pump, args=(server, client, False), daemon=True
            ).start()

    def pump(self, source, target, upstream):
        try:
            while not self.closed:
                data = source.recv(65536)
                if not data:
                    return
                if upstream:
                    with self.condition:
                        while self.paused and not self.closed:
                            self.condition.wait()
                        if self.closed:
                            return
                        target.sendall(data)
                else:
                    target.sendall(data)
        except OSError:
            pass
        finally:
            for connection in [source, target]:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                connection.close()

    def pause(self):
        with self.condition:
            self.paused = True

    def resume(self):
        with self.condition:
            self.paused = False
            self.condition.notify_all()

    def close(self):
        with self.condition:
            self.closed = True
            self.condition.notify_all()
        self.listener.close()
        for connection in self.connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()


class Pilot:
    def __init__(self, root, binaries, database):
        self.root, self.bin = root, binaries
        self.processes, self.logs = {}, []
        root.mkdir(parents=True, exist_ok=True)
        root.chmod(0o700)
        self.schema = "pilot_" + uuid.uuid4().hex
        parts = urllib.parse.urlsplit(database)
        pg = dict(
            os.environ,
            PGHOST=parts.hostname or "localhost",
            PGPORT=str(parts.port or 5432),
            PGUSER=urllib.parse.unquote(parts.username or "postgres"),
            PGPASSWORD=urllib.parse.unquote(parts.password or ""),
            PGDATABASE=parts.path.lstrip("/"),
        )
        subprocess.run(
            [
                "psql",
                "-X",
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                f"CREATE SCHEMA {self.schema}",
            ],
            env=pg,
            check=True,
            stdout=subprocess.DEVNULL,
        )
        query = urllib.parse.parse_qs(parts.query)
        assert "options" not in query, (
            "provide a database URL without search_path options"
        )
        query["options"] = ["-csearch_path=" + self.schema]
        self.env = dict(
            os.environ,
            ME_LEDGER_DATABASE_URL=urllib.parse.urlunsplit(
                parts._replace(query=urllib.parse.urlencode(query, doseq=True))
            ),
            RUST_LOG=os.environ.get("RUST_LOG", "info"),
        )
        self.pg_env = pg
        self.engine_port, self.gateway_port = port(), port()
        self.proxy = TLSProxy(self.engine_port)
        self.certs()
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
        self.engine_config = dict(
            listen=f"127.0.0.1:{self.engine_port}",
            data_directory=str(root / "engine"),
            tls=self.tls("matcher"),
            gateway_fingerprints=[self.fingerprint("gateway")],
            symbols=[symbol],
        )
        self.gateway_config = dict(
            listen=f"127.0.0.1:{self.gateway_port}",
            tls=self.tls("gateway"),
            matcher=dict(
                address=f"127.0.0.1:{self.proxy.port}",
                server_name="localhost",
                certificate_fingerprint=self.fingerprint("matcher"),
                tls=self.tls("gateway"),
            ),
            principals={
                self.fingerprint("admin"): dict(
                    all_accounts=True, permissions=["read", "funding", "admin"]
                ),
                self.fingerprint("trader"): dict(
                    all_accounts=True, permissions=["read", "trade"]
                ),
            },
            symbols=[symbol],
            fees={"0": dict(id=str(uuid.uuid4()), maker_bps=10, taker_bps=20)},
            assets=dict(BTC=8, USDT=8),
        )
        self.write_config()

    def write_config(self):
        (self.root / "matcher.json").write_text(json.dumps(self.engine_config))
        (self.root / "gateway.json").write_text(json.dumps(self.gateway_config))

    def certs(self):
        def openssl(*args):
            subprocess.run(
                ["openssl", *args],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

        openssl(
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
            "-days",
            "2",
            "-subj",
            "/CN=Financial-pilot-test-CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-keyout",
            str(self.root / "ca.key"),
            "-out",
            str(self.root / "ca.pem"),
        )
        for index, name in enumerate(
            ["matcher", "gateway", "admin", "trader", "unauthorized"], 1
        ):
            openssl(
                "req",
                "-new",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-subj",
                f"/CN={name}",
                "-keyout",
                str(self.root / (name + ".key")),
                "-out",
                str(self.root / (name + ".csr")),
            )
            ext = self.root / (name + ".ext")
            usage = (
                "serverAuth,clientAuth"
                if name == "gateway"
                else "serverAuth"
                if name == "matcher"
                else "clientAuth"
            )
            ext.write_text(
                f"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage={usage}\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n"
            )
            openssl(
                "x509",
                "-req",
                "-days",
                "2",
                "-in",
                str(self.root / (name + ".csr")),
                "-CA",
                str(self.root / "ca.pem"),
                "-CAkey",
                str(self.root / "ca.key"),
                "-set_serial",
                str(index),
                "-extfile",
                str(ext),
                "-out",
                str(self.root / (name + ".pem")),
            )
        for key in self.root.glob("*.key"):
            key.chmod(0o600)

    def tls(self, name):
        return dict(
            certificate=str(self.root / (name + ".pem")),
            private_key=str(self.root / (name + ".key")),
            ca=str(self.root / "ca.pem"),
        )

    def fingerprint(self, name):
        der = ssl.PEM_cert_to_DER_cert((self.root / (name + ".pem")).read_text())
        return hashlib.sha256(der).hexdigest()

    def start(self, name):
        log = (self.root / (name + ".log")).open("ab")
        self.logs.append(log)
        command = [
            str(self.bin / ("me-server" if name == "matcher" else "me-gateway")),
            "--financial-config" if name == "matcher" else "--config",
            str(self.root / ("matcher.json" if name == "matcher" else "gateway.json")),
        ]
        self.processes[name] = subprocess.Popen(
            command, env=self.env, stdout=log, stderr=subprocess.STDOUT
        )
        return self.processes[name]

    def kill(self, name):
        process = self.processes.pop(name, None)
        if process and process.poll() is None:
            process.kill()
            process.wait(timeout=10)

    def close(self):
        self.proxy.close()
        for name in list(self.processes):
            self.kill(name)
        for log in self.logs:
            log.close()

    def api(self, method, path, body=None, caller="admin", expected=200):
        context = ssl.create_default_context(cafile=str(self.root / "ca.pem"))
        context.load_cert_chain(
            str(self.root / (caller + ".pem")), str(self.root / (caller + ".key"))
        )
        connection = http.client.HTTPSConnection(
            "localhost", self.gateway_port, context=context, timeout=5
        )
        try:
            connection.request(
                method,
                path,
                None if body is None else json.dumps(body),
                {"Content-Type": "application/json"},
            )
            response = connection.getresponse()
            data = response.read()
            assert response.status == expected, (
                method,
                path,
                response.status,
                data.decode()[:500],
            )
            return json.loads(data) if data else None
        finally:
            connection.close()

    def ready(self):
        process = self.processes.get("gateway")
        if process and process.poll() is not None:
            raise AssertionError("gateway exited; inspect gateway.log")
        try:
            return self.api("GET", "/health/ready")["ready"]
        except (AssertionError, OSError, http.client.HTTPException) as e:
            (self.root / "not-ready.txt").write_text(str(e))
            return False

    def order(self, account, order):
        return self.api(
            "GET", f"/v1/accounts/{account}/orders/{order}", caller="trader"
        )


def run(args):
    database = os.environ["LEDGER_TEST_DATABASE_URL"]
    root = (
        Path(args.output) if args.output else Path(tempfile.mkdtemp(prefix="me-pilot-"))
    )
    pilot = Pilot(root, Path(args.bin_dir).resolve(), database)
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
        assert (
            pilot.api("POST", "/v1/admin/accounts", {"key": "buyer", "name": "buyer"})[
                "account_id"
            ]
            == buyer
        )
        for account, asset, amount in [(buyer, "USDT", "1000"), (seller, "BTC", "10")]:
            pilot.api(
                "POST",
                f"/v1/accounts/{account}/funding",
                dict(asset=asset, amount=amount, external_reference="seed-" + asset),
            )
        pilot.api(
            "POST",
            f"/v1/accounts/{buyer}/funding",
            dict(asset="USDT", amount="1", external_reference="forbidden"),
            caller="trader",
            expected=403,
        )
        original = {
            "key": "buy",
            "order": dict(
                symbol=0,
                side="buy",
                order_type="limit",
                time_in_force="GTC",
                price="100",
                quantity="1",
            ),
        }
        first = pilot.api(
            "POST",
            f"/v1/accounts/{buyer}/orders",
            original,
            caller="trader",
            expected=202,
        )
        wait(lambda: pilot.order(buyer, first["order_id"])["status"] == "open")
        before = (
            subprocess.check_output(
                [
                    "psql",
                    "-X",
                    "-A",
                    "-t",
                    "-c",
                    f"SELECT sequence FROM {pilot.schema}.cursors",
                ],
                env=pilot.pg_env,
            )
            .decode()
            .strip()
        )
        pilot.proxy.pause()
        sell = pilot.api(
            "POST",
            f"/v1/accounts/{seller}/orders",
            {
                "key": "sell",
                "order": dict(
                    symbol=0,
                    side="sell",
                    order_type="limit",
                    time_in_force="GTC",
                    price="90",
                    quantity="1",
                ),
            },
            caller="trader",
            expected=202,
        )
        pending = (
            subprocess.check_output(
                [
                    "psql",
                    "-X",
                    "-A",
                    "-t",
                    "-c",
                    f"SELECT count(*) FROM {pilot.schema}.outbox WHERE NOT completed",
                ],
                env=pilot.pg_env,
            )
            .decode()
            .strip()
        )
        after = (
            subprocess.check_output(
                [
                    "psql",
                    "-X",
                    "-A",
                    "-t",
                    "-c",
                    f"SELECT sequence FROM {pilot.schema}.cursors",
                ],
                env=pilot.pg_env,
            )
            .decode()
            .strip()
        )
        assert pending == "1" and before == after, (
            "the crash barrier must prove an unsubmitted durable outbox"
        )
        # SIGINT preserves a matcher barrier while the proxy proves the sell was never sent.
        pilot.processes["matcher"].send_signal(signal.SIGINT)
        assert pilot.processes["matcher"].wait(timeout=15) == 0
        pilot.kill("gateway")
        pilot.proxy.resume()
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

        def paused():
            try:
                return pilot.api("GET", "/health/ready", expected=503)
            except AssertionError:
                return None

        state = wait(paused, 60)
        assert state["ledger"]["reconciliation_reason"] is None
        pilot.api("POST", "/v1/admin/maintenance", {"enabled": False})
        wait(pilot.ready, 60)
        wait(lambda: pilot.order(seller, sell["order_id"])["status"] == "filled")
        assert (
            pilot.api(
                "POST",
                f"/v1/accounts/{buyer}/orders",
                original,
                caller="trader",
                expected=202,
            )
            == first
        )
        balances = {
            item["asset"]: item
            for item in pilot.api("GET", f"/v1/accounts/{buyer}/balances")
        }
        assert balances["USDT"]["available"] == "899.9", balances
        assert balances["USDT"]["reserved"] == "0", balances
        assert balances["BTC"]["available"] == "1", balances
        balances = {
            item["asset"]: item
            for item in pilot.api("GET", f"/v1/accounts/{seller}/balances")
        }
        assert balances["USDT"]["available"] == "99.8", balances
        assert balances["BTC"]["available"] == "9", balances
        events = pilot.api("GET", f"/v1/accounts/{buyer}/events")
        trades = [
            e for d in events["decisions"] for e in d["events"] if e["type"] == "trade"
        ]
        assert len(trades) == 1 and trades[0]["fee"] == "0.1", trades
        pilot.api(
            "POST",
            f"/v1/accounts/{buyer}/orders",
            {"key": "buy", "order": dict(original["order"], quantity="2")},
            caller="trader",
            expected=409,
        )
        pilot.api(
            "POST",
            f"/v1/accounts/{buyer}/orders",
            {
                "key": "unprotected",
                "order": dict(
                    symbol=0,
                    side="buy",
                    order_type="market",
                    time_in_force="IOC",
                    quantity="1",
                ),
            },
            caller="trader",
            expected=422,
        )
        # Another process must fail before it can dispatch the same outbox.
        log = (root / "second-gateway.log").open("wb")
        second = subprocess.Popen(
            [str(pilot.bin / "me-gateway"), "--config", str(root / "gateway.json")],
            env=pilot.env,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        assert second.wait(timeout=15) != 0
        log.close()
        barrier = pilot.api("POST", "/v1/admin/maintenance", {"enabled": True})
        pilot.api("GET", "/health/ready", expected=503)
        pilot.api("POST", "/v1/admin/reconcile")
        pilot.api("POST", "/v1/admin/maintenance", {"enabled": False})
        wait(pilot.ready)
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
        wait(pilot.ready, 60)
        balances = {
            item["asset"]: item
            for item in pilot.api("GET", f"/v1/accounts/{buyer}/balances")
        }
        assert (
            balances["USDT"]["available"] == "899.9"
            and balances["BTC"]["available"] == "1"
        )

        report = dict(
            status="passed",
            schema=pilot.schema,
            checks=[
                "mTLS funded path",
                "role enforcement",
                "atomic reservation and outbox",
                "both trade sides and fees",
                "real kill/restart",
                "idempotent original response",
                "account event feed",
                "exclusive gateway",
                "coordinated maintenance",
            ],
            barrier=barrier,
        )
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(dict(status="passed", report=str(root / "report.json"))))
    finally:
        pilot.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", default="target/debug")
    parser.add_argument("--output")
    run(parser.parse_args())
