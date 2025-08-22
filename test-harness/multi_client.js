const mc = require('minecraft-protocol');
const HOST = 'localhost';
const PORT = 25580;
const VERSION = '1.21.1';

let errors = 0;

function connect(name, delay) {
  return new Promise((resolve) => {
    setTimeout(() => {
      mc.ping({ host: HOST, port: PORT, version: VERSION, servername: HOST }, (err, res) => {
        if (err) {
          console.error('ping failed', name, err.message);
        } else {
          const motd = res.description?.text || res.description;
          console.log('ping', name, res.latency, motd);
        }
      });
      const client = mc.createClient({ host: HOST, port: PORT, username: name, version: VERSION, auth: 'offline', servername: HOST });
      client.on('login', () => {
        console.log('login', name);
        setTimeout(() => client.end('test complete'), 5000);
      });
      client.on('chat', (packet) => {
        try {
          const msg = JSON.parse(packet.message).text;
          console.log('chat', name, msg);
        } catch (_) {
          console.log('chat', name, packet.message);
        }
      });
      client.on('kick_disconnect', (packet) => {
        try {
          const msg = JSON.parse(packet.reason).text;
          console.log('kicked', name, msg);
        } catch (_) {
          console.log('kicked', name, packet.reason);
        }
      });
      client.on('end', () => {
        console.log('client ended', name);
        resolve();
      });
      client.on('error', (err) => {
        console.error('client error', name, err.message);
        errors++;
        resolve();
      });
    }, delay);
  });
}

const BOT_COUNT = parseInt(process.env.BOT_COUNT || '50', 10);

async function main() {
  const tasks = [];
  for (let i = 0; i < BOT_COUNT; i++) {
    tasks.push(connect(`bot${i}`, i * 20));
  }
  await Promise.all(tasks);
  if (errors > 0) {
    throw new Error(`${errors} clients failed`);
  }
}

main().catch(err => {
  console.error('test failed', err);
  process.exit(1);
});
