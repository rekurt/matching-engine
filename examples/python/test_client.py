import socket
import struct
import unittest
from client import MEClient


class ProtocolTests(unittest.TestCase):
    def test_command_has_v2_header_and_nonzero_request_id(self):
        sender, receiver = socket.socketpair()
        self.addCleanup(sender.close)
        self.addCleanup(receiver.close)
        client = MEClient()
        client._sock = sender
        client.cancel_order(42, 0)
        receiver.settimeout(1)
        data = receiver.recv(100)
        self.assertEqual(len(data), 32)
        self.assertEqual(struct.unpack('<IBBQQ', data[:22]), (10, 2, 2, 1, 1))
        self.assertEqual(data[22:], struct.pack('<QH', 42, 0))

    def test_v2_accepted_response_preserves_request_correlation(self):
        client = MEClient()
        payload = struct.pack('<QQ', 42, 123)
        client._buf = bytearray(struct.pack('<IBBQQ', len(payload), 2, 0x10, 7, 9) + payload)
        event = client._try_decode()
        self.assertEqual(event['order_id'], 42)
        self.assertEqual(event['seq'], 7)
        self.assertEqual(event['request_id'], 9)


if __name__ == '__main__':
    unittest.main()
