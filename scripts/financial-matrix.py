#!/usr/bin/env python3
"""Exercise funded order combinations and compare final balances to an independent trade model."""

import argparse
from decimal import Decimal, getcontext
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import time

_spec = importlib.util.spec_from_file_location(
    "financial_smoke", Path(__file__).with_name("financial-smoke.py")
)
_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_module)
Pilot, wait = _module.Pilot, _module.wait


def run(args):
    getcontext().prec = 100
    pilot = Pilot(
        Path(args.output).resolve(),
        Path(args.bin_dir).resolve(),
        os.environ["LEDGER_TEST_DATABASE_URL"],
    )
    number = 0
    orders = []
    cases = []
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
        accounts = {
            side: pilot.api("POST", "/v1/admin/accounts", {"key": side, "name": side})[
                "account_id"
            ]
            for side in ["buy", "sell"]
        }
        for account in accounts.values():
            for asset, amount in [("BTC", "10000"), ("USDT", "10000000")]:
                pilot.api(
                    "POST",
                    f"/v1/accounts/{account}/funding",
                    dict(
                        asset=asset, amount=amount, external_reference=account + asset
                    ),
                )

        def submit(side, **changes):
            nonlocal number
            number += 1
            key = f"matrix-{number}"
            spec = dict(
                symbol=0,
                side=side,
                order_type="limit",
                time_in_force="GTC",
                price="100",
                quantity="1",
            )
            spec.update(changes)
            result = pilot.api(
                "POST",
                f"/v1/accounts/{accounts[side]}/orders",
                {"key": key, "order": spec},
                caller="trader",
                expected=202,
            )
            wait(
                lambda: (
                    pilot.api(
                        "GET",
                        f"/v1/accounts/{accounts[side]}/requests/{key}",
                        caller="trader",
                    )["status"]
                    != "pending"
                )
            )
            orders.append((side, result["order_id"]))
            return result["order_id"]

        def clean():
            nonlocal number
            for side, order in orders:
                if pilot.order(accounts[side], order)["status"] == "open":
                    number += 1
                    key = f"cancel-{number}"
                    pilot.api(
                        "POST",
                        f"/v1/accounts/{accounts[side]}/orders/{order}/cancel",
                        {"key": key},
                        caller="trader",
                        expected=202,
                    )
                    wait(lambda: pilot.order(accounts[side], order)["status"] != "open")
            orders.clear()

        def restart():
            pilot.kill("gateway")
            pilot.kill("matcher")
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
            wait(pilot.ready, 60)

        for side in ["buy", "sell"]:
            opposite = "sell" if side == "buy" else "buy"
            for kind in [
                "limit",
                "market",
                "stop_limit",
                "stop_market",
                "trailing_stop_limit",
                "trailing_stop_market",
            ]:
                for tif in ["GTC", "IOC", "FOK", "GTD"]:
                    for flags in (
                        [
                            [],
                            ["post_only"],
                            ["maker_or_cancel"],
                            ["post_only", "maker_or_cancel"],
                        ]
                        if "limit" in kind
                        else [[]]
                    ):
                        clean()
                        fields = dict(
                            order_type=kind.replace("trailing_", ""),
                            time_in_force=tif,
                            flags=flags,
                            expire_at=time.time_ns() + 60_000_000_000
                            if tif == "GTD"
                            else 0,
                        )
                        if "market" in kind:
                            fields["protection_price"] = (
                                "110" if side == "buy" else "90"
                            )
                        if "stop" in kind:
                            fields["stop_price"] = (
                                "105"
                                if side == "buy" and kind.startswith("trailing_")
                                else "95"
                                if side == "sell" and kind.startswith("trailing_")
                                else "100"
                            )
                            if kind.startswith("trailing_"):
                                fields["trail_offset"] = "5"
                            order = submit(side, **fields)
                            held = Decimal(
                                pilot.order(accounts[side], order)["reserved"]
                            )
                            assert held > 0, ("unreserved stop", side, kind, tif, flags)
                            if kind.startswith("trailing_"):
                                first = "90" if side == "buy" else "110"
                                submit(opposite, price=first)
                                submit(side, price=first)
                                trigger = "95" if side == "buy" else "105"
                            else:
                                trigger = "100"
                            submit(opposite, price=trigger, quantity="2")
                            submit(side, price=trigger)
                            wait(
                                lambda: (
                                    pilot.order(accounts[side], order)["status"]
                                    != "open"
                                )
                            )
                        else:
                            if not flags:
                                submit(opposite, quantity="1")
                            order = submit(side, **fields)
                            state = pilot.order(accounts[side], order)
                            if not flags:
                                assert state["status"] == "filled", (kind, tif, state)
                        cases.append(dict(side=side, kind=kind, tif=tif, flags=flags))
            clean()
            iceberg = submit(side, quantity="3", display_quantity="1")
            expected_hold = Decimal("300.6") if side == "buy" else Decimal("3")
            assert (
                Decimal(pilot.order(accounts[side], iceberg)["reserved"])
                == expected_hold
            )
            submit(opposite, quantity="3")
            assert pilot.order(accounts[side], iceberg)["status"] == "filled"
            cases.append(dict(side=side, kind="iceberg", tif="GTC"))
            gtd = submit(
                side,
                price="1" if side == "buy" else "1000",
                time_in_force="GTD",
                expire_at=time.time_ns() + 1_000_000_000,
            )
            wait(lambda: pilot.order(accounts[side], gtd)["status"] == "cancelled", 10)
            cases.append(dict(side=side, kind="expiry", tif="GTD"))
        clean()
        buy = submit("buy", price="200", quantity="10")
        pilot.proxy.pause()
        old_fill = pilot.api(
            "POST",
            f"/v1/accounts/{accounts['sell']}/orders",
            {
                "key": "old-fill",
                "order": dict(
                    symbol=0,
                    side="sell",
                    order_type="limit",
                    time_in_force="GTC",
                    price="200",
                    quantity="5",
                ),
            },
            caller="trader",
            expected=202,
        )
        orders.append(("sell", old_fill["order_id"]))
        pilot.api(
            "POST",
            f"/v1/accounts/{accounts['buy']}/orders/{buy}/amend",
            {"key": "mixed-amend", "price": "150", "quantity": "20"},
            caller="trader",
            expected=202,
        )
        assert Decimal(pilot.order(accounts["buy"], buy)["reserved"]) == Decimal("3507")
        pilot.proxy.resume()
        wait(
            lambda: (
                pilot.api(
                    "GET",
                    f"/v1/accounts/{accounts['buy']}/requests/mixed-amend",
                    caller="trader",
                )["status"]
                == "accepted"
            )
        )
        amended = pilot.order(accounts["buy"], buy)
        assert (
            amended["version"] == 2
            and amended["remaining"] == "15"
            and amended["reserved"] == "2254.5"
        ), amended
        submit("sell", price="150", quantity="15")
        assert pilot.order(accounts["buy"], buy)["status"] == "filled"
        cases.append(
            dict(
                kind="mixed-amend",
                old="10@200",
                new="20@150",
                old_version_fill="5@200",
                new_version_fill="15@150",
            )
        )
        clean()
        for side in ["buy", "sell"]:
            opposite = "sell" if side == "buy" else "buy"
            iceberg = submit(
                side,
                quantity="3",
                display_quantity="1",
                time_in_force="GTD",
                expire_at=time.time_ns() + 60_000_000_000,
            )
            submit(opposite, quantity="1")
            assert pilot.order(accounts[side], iceberg)["remaining"] == "2"
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
            wait(pilot.ready, 60)
            submit(opposite, quantity="2")
            assert pilot.order(accounts[side], iceberg)["status"] == "filled"
            cases.append(dict(side=side, kind="iceberg-partial-crash", tif="GTD"))
            clean()
        pilot.proxy.pause()
        batch = [
            {
                "key": f"batch-{i}",
                "order": dict(
                    symbol=0,
                    side="buy",
                    order_type="limit",
                    time_in_force="GTC",
                    price=str(80 + i),
                    quantity="1",
                ),
            }
            for i in range(2)
        ]
        batch.append(
            {
                "key": "batch-invalid",
                "order": dict(
                    symbol=0,
                    side="buy",
                    order_type="limit",
                    time_in_force="GTC",
                    price="80",
                    quantity="0",
                ),
            }
        )
        result = pilot.api(
            "POST",
            f"/v1/accounts/{accounts['buy']}/batch/place",
            batch,
            caller="trader",
            expected=202,
        )
        assert (
            all("result" in item for item in result["results"][:2])
            and "error" in result["results"][2]
        ), result
        orders.extend(
            ("buy", item["result"]["order_id"]) for item in result["results"][:2]
        )
        restart()
        assert (
            pilot.api(
                "POST",
                f"/v1/accounts/{accounts['buy']}/batch/place",
                batch,
                caller="trader",
                expected=202,
            )
            == result
        )
        cases.append(dict(kind="batch-partial-results-crash", items=3, accepted=2))
        clean()
        trailing = submit(
            "buy",
            order_type="stop_market",
            price="0",
            stop_price="105",
            trail_offset="5",
            protection_price="100",
        )
        submit("buy", price="90")
        submit("sell", price="90")
        ask = submit("sell", price="95", quantity="2")
        pilot.proxy.pause()
        activation = pilot.api(
            "POST",
            f"/v1/accounts/{accounts['buy']}/orders",
            {
                "key": "activate-after-crash",
                "order": dict(
                    symbol=0,
                    side="buy",
                    order_type="limit",
                    time_in_force="GTC",
                    price="95",
                    quantity="1",
                ),
            },
            caller="trader",
            expected=202,
        )
        orders.append(("buy", activation["order_id"]))
        restart()
        wait(lambda: pilot.order(accounts["buy"], trailing)["status"] == "filled")
        assert pilot.order(accounts["sell"], ask)["status"] == "filled"
        cases.append(dict(kind="ratcheted-trailing-stop-activation-crash"))
        clean()
        expiry = submit(
            "buy",
            price="1",
            time_in_force="GTD",
            expire_at=time.time_ns() + 1_000_000_000,
        )
        pilot.kill("gateway")
        pilot.kill("matcher")
        time.sleep(1.2)
        restart()
        wait(lambda: pilot.order(accounts["buy"], expiry)["status"] == "cancelled")
        cases.append(dict(kind="GTD-expiry-during-downtime"))
        clean()
        barrier = pilot.api("POST", "/v1/admin/maintenance", {"enabled": True})
        pilot.api("POST", "/v1/admin/reconcile")
        sql = f"SELECT json_build_object('orders',(SELECT json_agg(json_build_object('id',id,'account',account_id,'side',spec->>'side')) FROM {pilot.schema}.orders),'decisions',(SELECT json_agg(payload ORDER BY sequence) FROM {pilot.schema}.decisions))"
        data = json.loads(
            subprocess.check_output(
                ["psql", "-X", "-A", "-t", "-c", sql], env=pilot.pg_env
            )
        )
        by_order = {o["id"]: o for o in data["orders"]}
        expected = {
            a: {"BTC": Decimal("10000"), "USDT": Decimal("10000000")}
            for a in accounts.values()
        }
        trades = 0
        for decision in data["decisions"]:
            for fill in decision["fills"]:
                trades += 1
                quantity = Decimal(fill["qty_raw"]) / Decimal(10**8)
                quote = (
                    Decimal(fill["price_raw"])
                    * Decimal(fill["qty_raw"])
                    / Decimal(10**16)
                )
                for role, bps in [("maker", 10), ("taker", 20)]:
                    order = by_order[fill[role + "_order"]]
                    wallet = expected[order["account"]]
                    fee = quote * Decimal(bps) / Decimal(10000)
                    if order["side"] == "buy":
                        wallet["BTC"] += quantity
                        wallet["USDT"] -= quote + fee
                    else:
                        wallet["BTC"] -= quantity
                        wallet["USDT"] += quote - fee
        for account, model in expected.items():
            for wallet in pilot.api("GET", f"/v1/accounts/{account}/balances"):
                assert Decimal(wallet["available"]) == model[wallet["asset"]], (
                    wallet,
                    model,
                )
                assert wallet["reserved"] == "0", wallet
        report = dict(
            status="passed",
            cases=cases,
            case_count=len(cases),
            model_trades=trades,
            barrier=barrier,
        )
        (pilot.root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(
            json.dumps(
                dict(status="passed", case_count=len(cases), model_trades=trades)
            )
        )
    finally:
        pilot.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--bin-dir", default="target/debug")
    run(parser.parse_args())
