import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "financial_backup", Path(__file__).with_name("financial-backup.py")
)
backup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backup)


class SnapshotAccountingTests(unittest.TestCase):
    def test_snapshot_checksum_rejects_changed_book_sequence(self):
        envelope = {
            "payload": {"watermark": {"sequence": 7}, "books": [{"sequence": 7}]},
            "digest": "d09a89000f9d93f3cf592428ca8a332ba1b439036d0f78d707fa64274286eceb",
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "snapshot.json"
            path.write_text(json.dumps(envelope, indent=2))
            self.assertEqual(backup.read_snapshot(path), envelope["payload"])
            envelope["payload"]["books"][0]["sequence"] = 8
            path.write_text(json.dumps(envelope))
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                backup.read_snapshot(path)

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
