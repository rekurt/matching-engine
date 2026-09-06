#!/usr/bin/env python3
"""Real-process recovery, delivery and startup regressions. Run after cargo build -p me-server."""
import json
import importlib.util
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import unittest
import urllib.request
import urllib.error

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('ME_SERVER_BIN', ROOT / 'target/debug/me-server')).resolve()


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Client:
    def __init__(self, port, sid=None, last=0, tls=None):
        self.sock = socket.create_connection(('127.0.0.1', port), 2)
        if tls is not None:
            self.sock = tls.wrap_socket(self.sock, server_hostname="127.0.0.1")
        self.seq = 0
        self.buf = b''
        self.last = last
        self.send(5 if sid is None else 6, 0, b'' if sid is None else sid + struct.pack('<Q', last))
        frames = self.drain()
        if sid is None:
            assert frames and frames[0][0] == 0x1a, frames
            payload = frames[0][3]
            offset = 1
            while offset < len(payload):
                tag, size = struct.unpack_from('<HH', payload, offset)
                offset += 4
                if tag == 2:
                    sid = payload[offset:offset + size]
                offset += size
        else:
            assert frames and frames[0][0] in (0x1b, 0x20), frames
        self.sid = sid
        self.replayed = frames

    def close(self):
        self.sock.close()

    def send(self, tag, rid, payload=b''):
        self.seq += 1
        self.sock.sendall(struct.pack('<IBBQQ', len(payload), 2, tag, self.seq, rid) + payload)

    def place(self, oid, side, rid, qty=1):
        self.send(1, rid, struct.pack('<QHBBBqqQ', oid, 0, side, 0, 0, 100 * 10**8, 0, qty * 10**8))
        return self.drain()

    def drain(self):
        self.sock.settimeout(.15)
        result = []
        while True:
            while len(self.buf) >= 22:
                n, version, tag, seq, rid = struct.unpack_from('<IBBQQ', self.buf)
                if len(self.buf) < 22 + n:
                    break
                assert version == 2
                result.append((tag, seq, rid, self.buf[22:22+n]))
                self.last = max(self.last, seq)
                self.buf = self.buf[22+n:]
            try:
                chunk = self.sock.recv(65536)
                if not chunk:
                    return result
                self.buf += chunk
            except socket.timeout:
                return result

    def open_orders(self, rid):
        self.send(9, rid)
        return [struct.unpack_from('<Q', frame[3])[0] for frame in self.drain() if frame[0] == 0x21]


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='me-runtime-')
        self.addCleanup(self.tmp.cleanup)
        self.directory = Path(self.tmp.name)
        self.port, self.http = free_port(), free_port()
        self.config = self.directory / 'config.toml'
        self.config.write_text(f'''[server]
tcp_addr="127.0.0.1:{self.port}"
metrics_addr="127.0.0.1:{self.http}"
shards=1
[server.admin]
api_key="runtime-test-admin-secret"
[wal]
enabled=true
mode="pre"
path="{self.directory}/wal"
snapshot_interval=2
[[symbols]]
id=0
base="BTC"
quote="USDT"
''')
        self.process = None
        self.log = open(self.directory / 'server.log', 'w+')
        self.addCleanup(self.log.close)
        self.addCleanup(self.stop)

    def start(self, env=None):
        self.process = subprocess.Popen([str(BINARY), '--config', str(self.config)], stdout=self.log, stderr=self.log, env=env)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.flush(); self.log.seek(0)
                self.fail('server exited: ' + self.log.read())
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:{self.http}/ready', timeout=.2) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.HTTPError):
                pass
            time.sleep(.02)
        self.fail('startup timeout')

    def stop(self, crash=False):
        if self.process is not None and self.process.poll() is None:
            (self.process.kill if crash else self.process.terminate)()
            self.process.wait(timeout=10)

    def client(self, sid=None, last=0):
        client = Client(self.port, sid, last)
        self.addCleanup(client.close)
        return client

    def test_maker_receives_execution_and_can_resume_after_process_crash(self):
        self.start()
        maker, taker = self.client(), self.client()
        self.assertIn(0x10, [f[0] for f in maker.place(1, 1, 100)])
        self.assertIn(0x12, [f[0] for f in taker.place(2, 0, 200)])
        events = maker.drain()
        self.assertIn(0x12, [f[0] for f in events])
        self.assertIn(0x13, [f[0] for f in events])
        maker.place(3, 1, 101)
        self.assertEqual(maker.open_orders(102), [3])
        sid, last = maker.sid, maker.last
        self.stop(crash=True)
        self.start()
        resumed = self.client(sid, last)
        self.assertEqual(resumed.open_orders(103), [3])
        resumed.send(2, 104, struct.pack('<QH', 3, 0))
        self.assertIn(0x15, [f[0] for f in resumed.drain()])

    def test_completed_order_retry_does_not_execute_again_after_crash(self):
        self.start()
        maker, taker = self.client(), self.client()
        maker.place(1, 1, 100)
        original = taker.place(2, 0, 200)
        self.assertIn(0x12, [f[0] for f in original])
        sid, last = taker.sid, taker.last
        self.stop(crash=True)
        self.start()
        resumed = self.client(sid, last)
        retry = resumed.place(2, 0, 200)
        self.assertEqual(retry, original)
        self.assertEqual(resumed.open_orders(201), [])

    def test_missing_session_journal_does_not_become_valid_on_second_start(self):
        self.start()
        client=self.client()
        self.assertIn(0x10,[f[0] for f in client.place(1,1,100)])
        self.stop(crash=True)
        journal=self.directory/'wal/sessions.wal'
        journal.unlink()
        for _ in range(2):
            process=subprocess.Popen([str(BINARY),'--config',str(self.config)],stdout=self.log,stderr=self.log)
            self.assertNotEqual(process.wait(timeout=10),0)
            self.assertFalse(journal.exists(), 'failed startup must not create replacement session history')

    def test_python_example_executes_order_and_cancel(self):
        if isinstance(self, TLSTests):
            self.skipTest('example intentionally demonstrates plain trusted TCP')
        self.start()
        spec=importlib.util.spec_from_file_location('me_example', ROOT/'examples/python/client.py')
        module=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        client=module.MEClient('127.0.0.1',self.port)
        self.addCleanup(client.close)
        client.connect(); client.handshake()
        client.place_order(17,0,1,price=100.0,qty=1.0)
        self.assertEqual(client.read_event()['type'],'OrderAccepted')
        client.cancel_order(17,0)
        self.assertEqual(client.read_event()['type'],'OrderCancelled')

    def test_graceful_shutdown_with_active_client_preserves_orders(self):
        self.start()
        client = self.client()
        self.assertIn(0x10, [f[0] for f in client.place(19, 1, 100)])
        sid, last = client.sid, client.last
        self.stop()
        self.assertEqual(self.process.returncode, 0)
        self.start()
        resumed = self.client(sid, last)
        self.assertEqual(resumed.open_orders(101), [19])

    def test_go_example_receives_cancellation_after_trade(self):
        if isinstance(self, TLSTests):
            self.skipTest('example demonstrates plain trusted TCP')
        self.start()
        result = subprocess.run(['go', 'run', '.', f'127.0.0.1:{self.port}'],
                                cwd=ROOT/'examples/go', capture_output=True,
                                text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Trade:', result.stdout)
        self.assertIn('OrderCancelled: order_id=1001', result.stdout)
        self.assertNotIn('ERROR', result.stderr)

    def test_default_logging_emits_audit_events(self):
        env = os.environ.copy()
        env.pop('RUST_LOG', None)
        self.start(env=env)
        client = self.client()
        self.assertIn(0x10, [f[0] for f in client.place(23, 1, 100)])
        self.stop()
        self.assertEqual(self.process.returncode, 0)
        self.log.seek(0)
        self.assertIn('"order_id":23', self.log.read())

    def test_admin_status_exposes_halt_and_resume(self):
        self.start()
        def request(path, method='GET'):
            req=urllib.request.Request(f'http://127.0.0.1:{self.http}{path}', method=method, headers={'X-Admin-Key':'runtime-test-admin-secret'})
            with urllib.request.urlopen(req, timeout=2) as response:
                return json.load(response)
        request('/admin/halt/0','POST')
        self.assertFalse(request('/admin/status')['symbols'][0]['trading_enabled'])
        request('/admin/resume/0','POST')
        self.assertTrue(request('/admin/status')['symbols'][0]['trading_enabled'])

    def test_unavailable_admin_listener_prevents_trading_start(self):
        with socket.socket() as blocker:
            blocker.bind(('127.0.0.1',self.http));blocker.listen()
            process=subprocess.Popen([str(BINARY),'--config',str(self.config)],stdout=self.log,stderr=self.log)
            self.assertNotEqual(process.wait(timeout=10),0)
            with self.assertRaises(OSError):
                socket.create_connection(('127.0.0.1', self.port), .1)


@unittest.skipUnless(os.environ.get('ME_TEST_TLS') == '1', 'requires binary built with --features tls')
class TLSTests(RuntimeTests):
    def setUp(self):
        super().setUp()
        def openssl(*args):
            subprocess.run(['openssl', *args], cwd=self.directory, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'ca.key', '-out', 'ca.pem', '-days', '1', '-subj', '/CN=Runtime Test CA', '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign')
        for name, usage in [('server', 'serverAuth'), ('client', 'clientAuth')]:
            openssl('req', '-newkey', 'rsa:2048', '-nodes', '-keyout', name+'.key', '-out', name+'.csr', '-subj', '/CN='+name)
            (self.directory / (name+'.ext')).write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nsubjectAltName=IP:127.0.0.1\nextendedKeyUsage='+usage+'\n')
            openssl('x509','-req','-in',name+'.csr','-CA','ca.pem','-CAkey','ca.key','-CAcreateserial','-out',name+'.pem','-days','1','-extfile',name+'.ext')
        with self.config.open('a') as config:
            config.write(f'\n[server.tls]\ncert_path="{self.directory}/server.pem"\nkey_path="{self.directory}/server.key"\nclient_ca_path="{self.directory}/ca.pem"\n')
        self.tls = ssl.create_default_context(cafile=str(self.directory/'ca.pem'))
        self.tls.load_cert_chain(str(self.directory/'client.pem'),str(self.directory/'client.key'))

    def client(self, sid=None, last=0):
        client=Client(self.port,sid,last,self.tls)
        self.addCleanup(client.close)
        return client

    def test_invalid_tls_files_fail_before_storage_or_listeners(self):
        self.config.write_text(self.config.read_text().replace('server.pem','missing.pem'))
        process=subprocess.Popen([str(BINARY),'--config',str(self.config)],stdout=self.log,stderr=self.log)
        self.assertNotEqual(process.wait(timeout=10),0)
        self.assertFalse((self.directory/'wal').exists())
        with self.assertRaises(OSError):
            socket.create_connection(('127.0.0.1',self.port),.1)

    def test_mtls_rejects_connection_without_client_certificate(self):
        self.start()
        context=ssl.create_default_context(cafile=str(self.directory/'ca.pem'))
        with socket.create_connection(('127.0.0.1',self.port),2) as raw:
            with self.assertRaises((ssl.SSLError, ConnectionError, AssertionError)):
                with context.wrap_socket(raw,server_hostname='127.0.0.1') as connection:
                    connection.sendall(struct.pack('<IBBQQ',0,2,5,1,0))
                    self.assertNotEqual(connection.recv(22),b'')


if __name__ == '__main__':
    unittest.main()
