const assert = require('node:assert/strict');
const net = require('node:net');
const {spawn} = require('node:child_process');
const {test} = require('node:test');

test('example sends protocol v2 handshake and correlated order', {timeout: 5000}, async (t) => {
  let child;
  const sockets = [];
  const server = net.createServer();
  t.after(() => { child?.kill(); sockets.forEach(s => s.destroy()); server.close(); });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const result = new Promise((resolve, reject) => {
    server.on('connection', socket => {
      sockets.push(socket);
      let data = Buffer.alloc(0);
      socket.on('data', part => {
        data = Buffer.concat([data, part]);
        try {
          if (data.length < 6) return;
          assert.equal(data[4], 2);
          while (data.length >= 22 && data.length >= 22 + data.readUInt32LE(0)) {
            const n = data.readUInt32LE(0);
            const frame = data.subarray(0, 22 + n);
            data = data.subarray(22 + n);
            if (frame[5] === 5) {
              const ack = Buffer.alloc(23);
              ack.writeUInt32LE(1, 0); ack[4] = 2; ack[5] = 0x1a;
              ack.writeBigUInt64LE(1n, 6); ack[22] = 2;
              socket.write(ack);
            } else {
              assert.equal(frame[5], 1);
              assert.ok(frame.readBigUInt64LE(14) > 0n);
              assert.equal(n, 37);
              resolve();
            }
          }
        } catch (error) { reject(error); }
      });
    });
  });
  child = spawn(process.execPath, ['dist/client.js', '127.0.0.1', String(server.address().port)], {stdio:'ignore'});
  await result;
});
