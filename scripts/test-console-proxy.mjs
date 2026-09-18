// Exercise the shipped Nginx configuration, without touching a running console.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {isIP} from 'node:net';
import {resolve, join} from 'node:path';
import {mkdtemp, copyFile, chmod, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {setTimeout as sleep} from 'node:timers/promises';
import {runCommand} from './bounded-process.mjs';

const name = `hibana-console-proxy-test-${process.pid}`;
const directory = await mkdtemp(join(tmpdir(), 'hibana-console-proxy-'));
const prepare = join(directory, 'prepare-config.sh');
await copyFile(resolve('console/prepare-config.sh'), prepare);
// Match COPY --chmod=755 in the console Dockerfile.
await chmod(prepare, 0o755);
const upstream = createServer((req, res) => {
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify({path:req.url, forwarded:req.headers['x-forwarded-for']}));
});
await new Promise(done => upstream.listen(0, '0.0.0.0', done));
let started = false;
try {
  await runCommand('docker', ['run', '--rm', '-d', '--name', name, '-p', '127.0.0.1::8080',
    '--add-host', 'host.docker.internal:host-gateway',
    '-e', `HIBANA_API_UPSTREAM=http://host.docker.internal:${upstream.address().port}`,
    '-e', 'NGINX_ENVSUBST_OUTPUT_DIR=/tmp/conf.d', '-e', 'NGINX_ENVSUBST_FILTER=HIBANA_API_UPSTREAM',
    '-v', `${resolve('console/nginx.conf')}:/etc/nginx/nginx.conf:ro`,
    '-v', `${resolve('console/default.conf.template')}:/etc/nginx/templates/default.conf.template:ro`,
    '-v', `${prepare}:/docker-entrypoint.d/05-hibana-config.sh:ro`,
    process.env.HIBANA_CONSOLE_TEST_IMAGE || 'nginxinc/nginx-unprivileged:1.28-alpine'], {timeoutMs:120000});
  started = true;
  const address = (await runCommand('docker', ['port', name, '8080/tcp'])).trim();
  const url = `http://${address}`;
  for (let i=0; ; i++) {
    try { if ((await fetch(url + '/healthz', {signal:AbortSignal.timeout(500)})).ok) break; } catch {}
    if (i === 50) {
      throw new Error('console Nginx did not become healthy: ' + await runCommand('docker', ['logs', name]));
    }
    await sleep(100);
  }
  for (const prefix of ['', '198.51.100.10', 'spoofed, 198.51.100.10, 10.20.1.2']) {
    const response = await fetch(url + '/api/auth/login', {
      headers:prefix ? {'x-forwarded-for':prefix} : {}, signal:AbortSignal.timeout(5000),
    });
    assert.equal(response.status, 200);
    const body = await response.json();
    assert.equal(body.path, '/auth/login');
    const chain = body.forwarded.split(',').map(ip => ip.trim());
    assert.ok(isIP(chain.pop()), 'console must append the immediate peer address');
    assert.deepEqual(chain, prefix ? prefix.split(',').map(ip => ip.trim()) : []);
  }
  console.log('PASS real console Nginx preserves the chain and appends the actual peer, including direct requests');
} finally {
  if (started) await runCommand('docker', ['stop', name]);
  upstream.closeAllConnections();
  await new Promise(done => upstream.close(done));
  await rm(directory, {recursive:true, force:true});
}
