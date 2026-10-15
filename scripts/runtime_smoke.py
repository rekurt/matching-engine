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
import threading
import time
import unittest
from unittest import mock
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
        self.events = []
        self.send(5 if sid is None else 6, 0, b'' if sid is None else sid + struct.pack('<Q', last))
        def handshake_complete(frames):
            first = frames[0]
            if sid is None or first[0] != 0x1b:
                return True
            return len(frames) >= 1 + struct.unpack('<I', first[3])[0]

        frames = self.drain(handshake_complete)
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
        return self.barrier()

    def barrier(self):
        self.send(4, 0)
        marker = struct.pack('<Q', self.seq)
        frames = self.drain(lambda frames: frames[-1][0] == 0x18 and frames[-1][3] == marker)
        return frames[:-1]

    def drain(self, complete=None, advance=True):
        deadline = time.monotonic() + 5 if complete else None
        result = []
        while True:
            while len(self.buf) >= 22:
                n, version, tag, seq, rid = struct.unpack_from('<IBBQQ', self.buf)
                if len(self.buf) < 22 + n:
                    break
                assert version == 2
                result.append((tag, seq, rid, self.buf[22:22+n]))
                if advance:
                    self.last = max(self.last, seq)
                self.buf = self.buf[22+n:]
                if complete and complete(result):
                    return result
            remaining = deadline - time.monotonic() if deadline else .15
            if remaining <= 0:
                raise TimeoutError('incomplete protocol response')
            self.sock.settimeout(remaining)
            try:
                chunk = self.sock.recv(65536)
                if not chunk:
                    if complete:
                        raise ConnectionError('connection closed before complete response')
                    return result
                self.buf += chunk
            except socket.timeout:
                if complete:
                    raise TimeoutError('incomplete protocol response')
                return result

    def open_orders(self, rid):
        self.send(9, rid)
        # A TLS write can split the snapshot across records; silence is not its end.
        frames = self.drain(lambda frames: frames[-1][0] == 0x19 or
                            (frames[-1][2] == rid and frames[-1][0] == 0x22), advance=False)
        errors = [f for f in frames if f[0] == 0x19]
        if errors:
            code, size = struct.unpack_from('<HH', errors[0][3])
            message = errors[0][3][4:4 + size].decode('utf-8')
            raise RuntimeError(f'snapshot error {code}: {message}')
        snapshot = [f for f in frames if f[2] == rid]
        assert snapshot[0][0] == 0x1f and snapshot[-1][0] == 0x22, snapshot
        count, begin_seq = struct.unpack('<IQ', snapshot[0][3])
        assert struct.unpack('<Q', snapshot[-1][3])[0] == begin_seq, snapshot
        orders = [struct.unpack_from('<Q', f[3])[0] for f in snapshot if f[0] == 0x21]
        assert len(orders) == count, snapshot
        self.events.extend(f for f in frames if f[2] != rid)
        self.last = max([self.last] + [f[1] for f in frames])
        return orders


class SnapshotClientTests(unittest.TestCase):
    def test_resume_waits_for_delayed_replay_and_rejects_early_eof(self):
        for truncated in (False, True):
            with self.subTest(truncated=truncated):
                local, remote = socket.socketpair()
                self.addCleanup(local.close)
                self.addCleanup(remote.close)
                sid = bytes(range(16))
                expected = [(0x1b, 0, 0, struct.pack('<I', 2)),
                            (0x12, 8, 0, b'trade'),
                            (0x18, 9, 0, struct.pack('<Q', 3))]
                errors = []

                def deliver():
                    try:
                        remote.settimeout(2)
                        request = b''
                        while len(request) < 46:
                            chunk = remote.recv(46 - len(request))
                            if not chunk:
                                raise ConnectionError('missing ResumeSession')
                            request += chunk
                        self.assertEqual(request[5], 6)
                        self.assertEqual(request[22:], sid + struct.pack('<Q', 7))
                        for tag, seq, rid, payload in expected[:2] if truncated else expected:
                            frame = struct.pack('<IBBQQ', len(payload), 2, tag, seq, rid) + payload
                            remote.sendall(frame[:10])
                            time.sleep(.2)
                            remote.sendall(frame[10:])
                        if truncated:
                            remote.shutdown(socket.SHUT_WR)
                    except Exception as error:
                        errors.append(error)

                sender = threading.Thread(target=deliver)
                sender.start()
                with mock.patch.object(socket, 'create_connection', return_value=local):
                    if truncated:
                        with self.assertRaises(ConnectionError):
                            Client(0, sid, 7)
                    else:
                        client = Client(0, sid, 7)
                        self.assertEqual(client.replayed, expected)
                        self.assertEqual(client.last, 9)
                sender.join(2)
                self.assertFalse(sender.is_alive())
                self.assertEqual(errors, [])

    def test_placement_waits_for_delayed_fragments_and_pong_barrier(self):
        local, remote = socket.socketpair()
        self.addCleanup(local.close)
        self.addCleanup(remote.close)
        client = Client.__new__(Client)
        client.sock, client.seq, client.last, client.buf, client.events = local, 0, 0, b'', []
        expected = [(0x10, 1, 101, b'accepted'), (0x13, 2, 101, b'filled')]
        errors = []

        def deliver():
            try:
                remote.settimeout(2)
                request = b''
                while len(request) < 81:  # Place header + payload, then Ping header.
                    chunk = remote.recv(81 - len(request))
                    if not chunk:
                        raise ConnectionError('connection closed before Place and Ping')
                    request += chunk
                self.assertEqual(struct.unpack_from('<IBBQQ', request, 59)[2], 4)
                for tag, seq, rid, payload in expected + [(0x18, 3, 0, struct.pack('<Q', 2))]:
                    frame = struct.pack('<IBBQQ', len(payload), 2, tag, seq, rid) + payload
                    remote.sendall(frame[:10])
                    time.sleep(.2)
                    remote.sendall(frame[10:])
            except Exception as error:
                errors.append(error)

        sender = threading.Thread(target=deliver)
        sender.start()
        self.addCleanup(sender.join, 2)
        self.assertEqual(client.place(1, 1, 101), expected)
        self.assertEqual(client.last, 3)
        sender.join(2)
        self.assertEqual(errors, [])

    def test_positive_control_sequences_advance_replay_cursor(self):
        for tag, seq, payload in [(0x1a, 11, b'\x02'), (0x18, 12, struct.pack('<Q', 101)),
                                  (0x20, 13, struct.pack('<QQ', 1, 2)),
                                  (0x19, 14, struct.pack('<HH', 12, 0)),
                                  (0x1b, 0, struct.pack('<I', 0))]:
            with self.subTest(tag=tag):
                local, remote = socket.socketpair()
                self.addCleanup(local.close)
                self.addCleanup(remote.close)
                client = Client.__new__(Client)
                client.sock, client.seq, client.last, client.buf, client.events = local, 0, 0, b'', []
                remote.sendall(struct.pack('<IBBQQ', len(payload), 2, tag, seq, 0) + payload)
                remote.shutdown(socket.SHUT_WR)
                client.drain()
                self.assertEqual(client.last, seq)

    def test_snapshot_reports_connection_scoped_error(self):
        local, remote = socket.socketpair()
        self.addCleanup(local.close)
        self.addCleanup(remote.close)
        client = Client.__new__(Client)
        client.sock, client.seq, client.last, client.buf, client.events = local, 0, 0, b'', []
        message = b'shard unavailable'
        payload = struct.pack('<HH', 12, len(message)) + message
        remote.sendall(struct.pack('<IBBQQ', len(payload), 2, 0x19, 0, 0) + payload)
        remote.shutdown(socket.SHUT_WR)
        with self.assertRaisesRegex(RuntimeError, 'snapshot error 12: shard unavailable'):
            client.open_orders(102)

    def test_snapshot_preserves_passive_events(self):
        local, remote = socket.socketpair()
        self.addCleanup(local.close)
        self.addCleanup(remote.close)
        client = Client.__new__(Client)
        client.sock, client.seq, client.last, client.buf, client.events = local, 0, 0, b'', []
        trade = (0x12, 1, 0, bytes(59))
        frames = [trade, (0x1f, 2, 102, struct.pack('<IQ', 1, 7)),
                  (0x21, 3, 102, struct.pack('<Q', 3) + bytes(52)),
                  (0x22, 4, 102, struct.pack('<Q', 7))]
        for tag, seq, rid, payload in frames:
            remote.sendall(struct.pack('<IBBQQ', len(payload), 2, tag, seq, rid) + payload)
        self.assertEqual(client.open_orders(102), [3])
        self.assertEqual(client.events, [trade])
        self.assertEqual(client.last, 4)

    def test_failed_snapshot_does_not_commit_cursor(self):
        for failure in ['missing_end', 'bad_count', 'bad_marker']:
            with self.subTest(failure=failure):
                local, remote = socket.socketpair()
                self.addCleanup(local.close)
                self.addCleanup(remote.close)
                client = Client.__new__(Client)
                client.sock, client.seq, client.last, client.buf, client.events = local, 0, 0, b'', []
                frames = [(0x1f, 1, struct.pack('<IQ', 1, 7))]
                if failure == 'bad_marker':
                    frames.append((0x21, 2, struct.pack('<Q', 3) + bytes(52)))
                if failure != 'missing_end':
                    frames.append((0x22, 3, struct.pack('<Q', 8 if failure == 'bad_marker' else 7)))
                for tag, seq, payload in frames:
                    remote.sendall(struct.pack('<IBBQQ', len(payload), 2, tag, seq, 102) + payload)
                remote.shutdown(socket.SHUT_WR)
                error = ConnectionError if failure == 'missing_end' else AssertionError
                with self.assertRaises(error):
                    client.open_orders(102)
                self.assertEqual(client.last, 0)

    def test_snapshot_waits_for_delayed_fragmented_end(self):
        local, remote = socket.socketpair()
        self.addCleanup(local.close)
        self.addCleanup(remote.close)
        client = Client.__new__(Client)
        client.sock, client.seq, client.last, client.buf, client.events = local, 0, 0, b'', []

        def frame(tag, seq, payload):
            return struct.pack('<IBBQQ', len(payload), 2, tag, seq, 102) + payload

        response = (frame(0x1f, 1, struct.pack('<IQ', 1, 7))
                    + frame(0x21, 2, struct.pack('<Q', 3) + bytes(52))
                    + frame(0x22, 3, struct.pack('<Q', 7)))
        errors = []

        def deliver():
            try:
                remote.settimeout(2)
                remote.recv(22)
                remote.sendall(response[:10])
                time.sleep(.25)
                remote.sendall(response[10:-5])
                time.sleep(.25)
                remote.sendall(response[-5:])
            except Exception as error:
                errors.append(error)

        sender = threading.Thread(target=deliver)
        sender.start()
        self.addCleanup(sender.join, 2)
        self.assertEqual(client.open_orders(102), [3])
        sender.join(2)
        self.assertFalse(sender.is_alive())
        self.assertEqual(errors, [])


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
        events = maker.barrier()
        self.assertIn(0x12, [f[0] for f in events])
        self.assertIn(0x13, [f[0] for f in events])
        self.assertIn(0x10, [f[0] for f in maker.place(3, 1, 101)])
        self.assertEqual(maker.open_orders(102), [3])
        sid, last = maker.sid, maker.last
        self.stop(crash=True)
        self.start()
        resumed = self.client(sid, last)
        self.assertEqual(resumed.open_orders(103), [3])
        resumed.send(2, 104, struct.pack('<QH', 3, 0))
        self.assertIn(0x15, [f[0] for f in resumed.barrier()])

    def test_ping_replay_cursor_survives_crash(self):
        self.start()
        client = self.client()
        handshake_seq = client.last
        self.assertGreater(handshake_seq, 0)
        client.seq = 100
        client.send(4, 0)
        pong = [f for f in client.drain(lambda frames: frames[-1][0] == 0x18) if f[0] == 0x18]
        self.assertEqual(len(pong), 1)
        self.assertEqual(struct.unpack('<Q', pong[0][3])[0], client.seq)
        self.assertGreater(pong[0][1], handshake_seq)
        self.assertNotEqual(pong[0][1], client.seq)
        self.assertEqual(client.last, pong[0][1])
        self.stop(crash=True)
        self.start()
        resumed = self.client(client.sid, client.last)
        self.assertEqual([f[0] for f in resumed.replayed], [0x1b])
        self.assertEqual(struct.unpack('<I', resumed.replayed[0][3])[0], 0)
        self.assertEqual(resumed.open_orders(102), [])

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

    def test_recovering_go_sdk_replays_committed_command(self):
        self.start()
        env=dict(os.environ,ME_RECOVERY_ENDPOINT=f'127.0.0.1:{self.port}',
                 ME_RECOVERY_CA=str(self.directory/'ca.pem'),ME_RECOVERY_CERT=str(self.directory/'client.pem'),ME_RECOVERY_KEY=str(self.directory/'client.key'))
        result=subprocess.run(['go','test','-race','-run','^TestRecoveringRuntimeTLS$','-count=1','-timeout','45s'],
                              cwd=ROOT/'clients/go/meclient',env=env,capture_output=True,text=True,timeout=60)
        self.assertEqual(result.returncode,0,result.stdout+result.stderr)

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
