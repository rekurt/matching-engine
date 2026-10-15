import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "financial_backup", Path(__file__).with_name("financial-backup.py")
)
backup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backup)


class SnapshotAccountingTests(unittest.TestCase):
    def fixture(self):
        account = "00112233-4455-6677-8899-aabbccddeeff"
        order_spec = dict(
            side="buy",
            symbol=0,
            order_type=0,
            qty_raw=300000000,
            price_raw=10000000000,
            protection_price_raw=10000000000,
        )
        meta = dict(
            account_id=account,
            version=1,
            fee_schedule="fee-v1",
            reservation_id="intent-1",
            spec=order_spec,
            status="open",
        )
        book = dict(
            id=1,
            qty=300000000,
            cumulative_qty=100000000,
            remaining_qty=100000000,
            display_qty=100000000,
            client_session_id=list(bytes.fromhex(account.replace("-", ""))),
            side="Buy",
            symbol=0,
        )
        snapshot = dict(
            orders={"1": meta}, books=[dict(resting_orders=[book], stop_orders=[])]
        )
        row = dict(
            id=1,
            **copy.deepcopy(meta),
            remaining="200000000",
            held=str(20040000000000000000000),
            maker_bps=10,
            taker_bps=20,
        )
        return snapshot, [row]

    def test_hidden_remaining_uses_frozen_fee_reserve(self):
        snapshot, rows = self.fixture()
        backup.verify_orders(snapshot, rows)
        rows[0]["held"] = str(int(rows[0]["held"]) - 1)
        with self.assertRaises(RuntimeError):
            backup.verify_orders(snapshot, rows)

    def test_missing_book_order_and_wrong_owner_fail(self):
        snapshot, rows = self.fixture()
        snapshot["books"][0]["resting_orders"][0]["client_session_id"] = [0] * 16
        with self.assertRaises(RuntimeError):
            backup.verify_orders(snapshot, rows)
        snapshot, rows = self.fixture()
        snapshot["books"][0]["resting_orders"] = []
        with self.assertRaises(RuntimeError):
            backup.verify_orders(snapshot, rows)

    def test_changed_version_and_closed_reserve_fail(self):
        snapshot, rows = self.fixture()
        rows[0]["version"] = 2
        with self.assertRaises(RuntimeError):
            backup.verify_orders(snapshot, rows)
        snapshot, rows = self.fixture()
        snapshot["orders"]["1"]["status"] = "cancelled"
        rows[0]["status"] = "cancelled"
        with self.assertRaises(RuntimeError):
            backup.verify_orders(snapshot, rows)
