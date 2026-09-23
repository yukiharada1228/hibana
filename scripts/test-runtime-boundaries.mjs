import { issueFixtureToken } from "./test-api-credentials.mjs";
// Uses the disposable database and real CP/Worker/Wasm from test-http.sh.
import assert from 'node:assert/strict';
import {createServer, request} from 'node:http';
import {connect} from 'node:net';
import {setTimeout as sleep} from 'node:timers/promises';
import {testLiveTail} from './test-live-tail.mjs';

export async function testRuntimeBoundaries({api, sql, token, wasm, upload, url, internal, metricsUrl, startWorker, stopWorker}) {
  let rejectResults = false, failures = 0;
  let stalled;
  const proxy = createServer((req, res) => {
    if (rejectResults && req.url === '/internal/direct-result') {
      failures++; req.resume(); res.writeHead(503).end(); return;
    }
    const upstream = request(internal + req.url, {method:req.method, headers:req.headers}, response => {
      res.writeHead(response.statusCode, response.headers); response.pipe(res);
    });
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    req.pipe(upstream);
  });
  await new Promise(done => proxy.listen(0, '127.0.0.1', done));
  const invoke = (method, path) => new Promise((done, reject) => {
    const req = request(url + path, {method, headers:{Host:'runtime-boundaries.upload.hibana.test'}}, res => {
      const chunks = [];
      res.on('data', chunk => chunks.push(chunk));
      res.on('end', () => done({status:res.statusCode, body:Buffer.concat(chunks).toString(), error:null}));
      res.on('error', error => done({status:res.statusCode, error}));
    });
    req.setTimeout(10000, () => req.destroy(new Error('Runtime boundary test deadline')));
    req.on('error', reject); req.end();
  });
  try {
    await stopWorker();
    await startWorker(`http://127.0.0.1:${proxy.address().port}`, {TOKIO_WORKER_THREADS:'1', WORKER_MAX_CONCURRENCY:'1'});
    const created = await api('/components', {token, method:'POST', body:{name:'runtime-boundaries'}});
    assert.equal(created.status, 201);
    const {component_id:id} = await created.json();
    const resource_limits = {max_memory_bytes:128*1024*1024, max_wall_time_ms:1000, max_execution_time_ms:1500};
    assert.equal((await upload(id, token, '1', wasm, 0, {ingress:true, resource_limits})).status, 201);
    const history = async () => (await (await api(`/components/${id}/executions`, {token})).json()).items;
    async function waitForTerminal() {
      for (let i=0; i<100; i++) {
        const latest = (await history())[0];
        if (latest && !['pending', 'running'].includes(latest.status)) return latest;
        await sleep(20);
      }
      assert.fail('execution did not reach a terminal state');
    }
    const trapStart = performance.now();
    assert.equal((await invoke('GET', '/trap')).status,502);
    const trapped = await waitForTerminal();
    assert.equal(trapped.status,'failed');
    assert.match(JSON.stringify(trapped.error),/wasm trap/);
    assert.doesNotMatch(JSON.stringify(trapped.error),/runtime regression fixture/);
    assert.ok(performance.now()-trapStart < 1000,'an immediate trap must not wait for the execution deadline');
    const detail = async id => (await (await api(`/executions/${id}`, {token})).json());
    assert.match((await detail(trapped.execution_id)).logs.stderr, /runtime regression fixture/);
    console.log('PASS trapped Wasm retains tenant-private stderr');
    assert.equal((await invoke('GET', '/logs')).body, 'logged');
    const logged = await waitForTerminal();
    const logs = await detail(logged.execution_id);
    assert.equal(logs.logs.stdout, 'hello 雪\n');
    assert.equal(logs.logs.stderr, 'fixture stderr\n');
    assert.equal(logs.logs.truncated, false);
    const reader = await (await issueFixtureToken(sql, {tenant_slug:'upload',email:'test@example.invalid',scopes:['read']})).json();
    const deployer = await (await issueFixtureToken(sql, {tenant_slug:'upload',email:'test@example.invalid',scopes:['deploy']})).json();
    for (const path of [`/executions/${logged.execution_id}`, `/components/${id}/logs`, `/components/${id}/tail`]) {
      assert.equal((await api(path)).status, 401);
      assert.equal((await api(path, {token:deployer.token})).status, 403);
      const response = await api(path, {token:reader.token});
      assert.equal(response.status, 200);
      assert.equal(response.headers.get('cache-control'), 'no-store');
    }
    const stored = await (await api(`/components/${id}/logs`, {token:reader.token})).json();
    assert.deepEqual(stored.items[0].logs, logs.logs);
    assert.equal(stored.items[0].version_id, logs.version_id);
    assert.equal((await invoke('GET', '/log-overflow')).body, 'overflow survived');
    const overflow = await detail((await waitForTerminal()).execution_id);
    assert.equal(overflow.status, 'succeeded');
    assert.equal(overflow.logs.truncated, true);
    assert.equal(Buffer.byteLength(overflow.logs.stdout) + Buffer.byteLength(overflow.logs.stderr), 16 * 1024);
    const logMetrics = await (await fetch(metricsUrl+'/metrics')).text();
    assert.match(logMetrics,/^faas_guest_log_dropped_bytes_total [1-9][0-9]*$/m);
    console.log('PASS stdout/stderr, Unicode, overflow without guest failure, Read scope and stored log retrieval');
    await testLiveTail({url, token:reader.token, invoke});

    for (const path of ['/header-limit', '/header-resources']) {
      assert.equal((await invoke('GET', path)).status,502);
      assert.equal((await waitForTerminal()).status,'failed');
      assert.equal((await invoke('GET', '/')).status,200);
    }
    console.log('PASS real Wasm cannot retain oversized fields or exceed the host resource limit; execution capacity recovers');

    const busy = invoke('GET', '/busy');
    busy.catch(() => {});
    await sleep(100);
    const healthStart = performance.now();
    const health = await fetch(metricsUrl+'/healthz', {signal:AbortSignal.timeout(3000)});
    assert.equal(health.status,200);
    await health.text();
    const healthMs = Math.round(performance.now()-healthStart);
    assert.equal((await busy).status,502);
    const timed = await waitForTerminal();
    assert.equal(timed.status,'timeout');
    assert.match((await detail(timed.execution_id)).logs.stderr, /before timeout/);
    assert.ok(healthMs < 500,`CPU-bound Wasm blocked the single-thread executor for ${healthMs} ms`);
    console.log(`PASS CPU-bound Wasm yields on a single executor thread; health answered in ${healthMs} ms`);

    const target = new URL(url);
    stalled = connect({host:target.hostname, port:Number(target.port)});
    await new Promise((done, reject) => { stalled.once('connect',done); stalled.once('error',reject); });
    stalled.on('error', () => {});
    stalled.write('GET /large HTTP/1.1\r\nHost: runtime-boundaries.upload.hibana.test\r\nConnection: close\r\n\r\n');
    stalled.pause();
    // Keep the client connected and unread past the 1.5-second execution deadline.
    await sleep(4500);
    const timedOut = (await history())[0];
    assert.equal(timedOut.status,'timeout');
    const metrics = await (await fetch(metricsUrl+'/metrics')).text();
    assert.match(metrics,/^wasmtime_inflight_executions 0$/m);
    assert.match(metrics,/^hibana_worker_guest_memory_reserved_bytes 0$/m);
    assert.equal((await invoke('GET', '/')).status,200,'the only execution slot must be reusable before client disconnect');
    stalled.destroy(); stalled = undefined;
    console.log('PASS stalled client: timeout persisted, memory/slot released, next request succeeds before disconnect');
    assert.equal(JSON.parse((await invoke('GET', '/dns-approved')).body).ok, false);
    assert.equal((await api(`/components/${id}/egress`, {token, method:'PATCH', body:{allow:['1.1.1.1:443']}})).status, 200);
    const approved = JSON.parse((await invoke('GET', '/dns-approved')).body);
    assert.deepEqual(approved, {ok:true, addresses:['1.1.1.1:443']});
    assert.equal(JSON.parse((await invoke('GET', '/dns-blocked')).body).ok, false);
    assert.equal((await api(`/components/${id}/egress`, {token, method:'PATCH', body:{deny:['1.1.1.1:443']}})).status, 200);
    assert.equal(JSON.parse((await invoke('GET', '/dns-approved')).body).ok, false, 'cached Wasm must read revoked application permissions on its next execution');
    console.log('PASS real Wasm DNS: application approval resolves, unrelated localhost stays blocked, revocation denies the next execution');

    const cases = [['HEAD', '/', 200], ['GET', '/empty', 204], ['GET', '/unchanged', 304]];
    for (const [method, path, status] of cases) {
      const result = await invoke(method, path);
      assert.equal(result.status, status);
      assert.equal(result.error, null);
      assert.equal(result.body, '');
    }
    rejectResults = true;
    for (const [method, path] of cases) {
      const result = await invoke(method, path);
      assert.equal(result.status, 502, `${method} ${path} must not acknowledge failed persistence`);
      assert.equal(result.error, null);
    }
    const streamed = await invoke('GET', '/');
    assert.equal(streamed.status, 200);
    assert.ok(streamed.error, 'ordinary streamed replies must terminate with an error');
    assert.equal(failures, 12, 'each execution retries persistence three times without rerunning Wasm');
    rejectResults = false;
    const recovered = await invoke('HEAD', '/');
    assert.equal(recovered.status, 200);
    assert.equal(recovered.error, null);
    console.log('PASS HEAD/204/304 wait for durable results; persistence failures produce 502, streaming errors, and recovery succeeds');
  } finally {
    stalled?.destroy();
    await stopWorker();
    proxy.closeAllConnections();
    await new Promise(done => proxy.close(done));
  }
}
