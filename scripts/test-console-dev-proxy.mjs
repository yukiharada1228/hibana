// Exercise the shipped Vite proxy and logger with a real failing upstream.
import assert from 'node:assert/strict';
import {createServer as createHttpServer} from 'node:http';
import {resolve} from 'node:path';
import {createServer} from '../console/node_modules/vite/dist/node/index.js';

const upstream = createHttpServer((req, res) => {
  if (req.url.includes('fail_upstream=true')) return req.socket.destroy();
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify({path:req.url}));
});
await new Promise(done => upstream.listen(0, '127.0.0.1', done));
const previousUpstream = process.env.HIBANA_API_UPSTREAM;
process.env.HIBANA_API_UPSTREAM = `http://127.0.0.1:${upstream.address().port}`;
const originalError = console.error;
const errors = [];
console.error = (...args) => errors.push(args.join(' '));
let server;
try {
  server = await createServer({
    root:resolve('console'), configFile:resolve('console/vite.config.ts'),
    clearScreen:false,
    server:{host:'127.0.0.1', port:0, strictPort:false},
  });
  await server.listen();
  const url = `http://127.0.0.1:${server.httpServer.address().port}`;
  const get = path => fetch(url + path, {signal:AbortSignal.timeout(5000)});
  const callback = '/api/auth/oidc/callback?code=fixture-private-provider-code&state=fixture-private-state';
  assert.equal((await (await get(callback)).json()).path, callback.slice(4));
  assert.equal((await get(callback + '&fail_upstream=true')).status, 500);
  assert.equal((await get('/api/diagnostic?fail_upstream=true')).status, 500);
  const logs = errors.join('\n');
  assert.ok(logs.includes('/auth/oidc/callback'), 'callback errors retain the route for diagnosis');
  assert.ok(logs.includes('/diagnostic?fail_upstream=true'), 'ordinary errors retain their diagnostics');
  assert.ok(logs.includes('socket hang up'), 'upstream failure reasons remain visible');
  assert.ok(!logs.includes('fixture-private-'), 'OIDC callback codes/state must not reach Vite logs');
  console.log('PASS development proxy forwards OIDC queries but omits credentials from upstream error logs');
} finally {
  console.error = originalError;
  if (previousUpstream === undefined) delete process.env.HIBANA_API_UPSTREAM;
  else process.env.HIBANA_API_UPSTREAM = previousUpstream;
  await server?.close();
  upstream.closeAllConnections();
  await new Promise(done => upstream.close(done));
}
