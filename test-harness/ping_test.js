const mc = require('minecraft-protocol');
const HOST = 'localhost';
const PORT = 25580;
const VERSION = '1.16.4';

mc.ping({ host: HOST, port: PORT, version: VERSION, servername: HOST }, (err, res) => {
  if (err) {
    console.error('ping failed', err.message);
    process.exit(1);
  } else {
    console.log('ping latency', res.latency, 'ms');
    console.log('ping motd', res.description?.text || res.description);
    process.exit(0);
  }
});
