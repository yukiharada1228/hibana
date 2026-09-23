// Runs only against the disposable database and real CP/Worker/Wasm in test-http.sh.
import assert from 'node:assert/strict';
import {createServer, request} from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';

export async function testExecutionShutdown({api, token, wasm, upload, url, internal, metricsUrl, startWorker, stopWorker}) {
  let capture = false, held, release, tenantId;
  const proxy = createServer(async (req, res) => {
    let bytes;
    if (capture && req.url === '/internal/direct-result') {
      capture = false;
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      bytes = Buffer.concat(chunks);
      held = JSON.parse(bytes);
      await new Promise(done => { release = done; });
      if (res.destroyed) return;
    }
    const upstream = request(internal + req.url, {method:req.method, headers:req.headers}, response => {
      res.writeHead(response.statusCode, response.headers); response.pipe(res);
    });
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    if (bytes) upstream.end(bytes); else req.pipe(upstream);
  });
  await new Promise(done => proxy.listen(0, '127.0.0.1', done));
  const proxyUrl = `http://127.0.0.1:${proxy.address().port}`;
  const invoke = path => new Promise((done, reject) => {
    const req = request(url + path, {headers:{Host:'execution-shutdown.upload.hibana.test'}}, res => {
      res.resume();
      res.on('end', () => done({status:res.statusCode, error:null}));
      res.on('error', error => done({status:res.statusCode, error}));
    });
    req.setTimeout(10000, () => req.destroy(new Error('Shutdown fixture deadline')));
    req.on('error', reject); req.end();
  });
  async function until(predicate, message) {
    for (let i=0; i<100; i++) {
      if (await predicate()) return;
      await sleep(50);
    }
    assert.fail(message);
  }
  const arm = () => { held = undefined; release = undefined; capture = true; };
  const tenantStatus = status => api(`/admin/tenants/${tenantId}/status`, {token:'test-only', method:'PUT', body:{status}});
  try {
    await stopWorker();
    let worker = await startWorker(proxyUrl, {TOKIO_WORKER_THREADS:'1', WORKER_DRAIN_TIMEOUT_SECS:'5'});
    tenantId = (await (await api('/auth/session', {token})).json()).tenant_id;
    const created = await api('/components', {token, method:'POST', body:{name:'execution-shutdown'}});
    assert.equal(created.status, 201);
    const {component_id:id} = await created.json();
    assert.equal((await upload(id, token, '1', wasm, 0, {ingress:true})).status, 201);
    const saved = async () => (await (await api(`/components/${id}/executions`, {token})).json()).items.find(row => row.execution_id === held.execution_id);

    arm();
    const pending = invoke('/'); pending.catch(() => {});
    await until(() => held, 'success result did not reach the persistence proxy');
    assert.equal(held.status, 'succeeded');
    assert.equal((await tenantStatus('suspended')).status, 200);
    const denied = await fetch(internal + '/internal/direct-job', {
      method:'POST', headers:{'x-hibana-job-token':held.job_token}, signal:AbortSignal.timeout(5000),
    });
    assert.equal(denied.status, 403, 'suspension must still prevent redemption');
    await denied.text();
    release();
    assert.deepEqual(await pending, {status:200, error:null});
    // Results are idempotent even while admission is suspended.
    const duplicate = await fetch(internal + '/internal/direct-result', {
      method:'POST', headers:{'x-hibana-job-token':held.job_token, 'content-type':'application/json'},
      body:JSON.stringify(held), signal:AbortSignal.timeout(5000),
    });
    assert.equal(duplicate.status, 204);
    assert.equal((await tenantStatus('active')).status, 200);
    assert.equal((await saved()).status, 'succeeded');
    console.log('PASS tenant suspension: admitted success persists and retries are accepted; redemption stays forbidden');

    arm();
    assert.equal((await invoke('/trap')).status, 502);
    await until(() => held, 'failed result did not reach the persistence proxy');
    assert.equal(held.status, 'failed');
    worker.kill('SIGTERM');
    await sleep(300);
    assert.equal(worker.exitCode, null, 'Worker must wait for result persistence after an early HTTP failure');
    assert.equal(worker.signalCode, null);
    assert.equal((await fetch(metricsUrl + '/readyz')).status, 503);
    assert.match(await (await fetch(metricsUrl + '/metrics')).text(), /^wasmtime_inflight_executions 1$/m);
    release();
    await until(() => worker.exitCode !== null || worker.signalCode !== null, 'Worker failed to exit after persistence');
    assert.equal(worker.exitCode, 0);
    assert.equal((await saved()).status, 'failed');
    console.log('PASS SIGTERM waits beyond early 502 until the detached execution result is saved');

    worker = await startWorker(proxyUrl, {TOKIO_WORKER_THREADS:'1', WORKER_DRAIN_TIMEOUT_SECS:'1'});
    arm();
    assert.equal((await invoke('/trap')).status, 502);
    await until(() => held, 'deadline result did not reach the persistence proxy');
    const started = performance.now();
    worker.kill('SIGTERM');
    await until(() => worker.exitCode !== null || worker.signalCode !== null, 'Worker ignored its drain deadline');
    const elapsed = performance.now() - started;
    assert.equal(worker.exitCode, 0);
    assert.ok(elapsed >= 900 && elapsed < 3000, `drain deadline took ${elapsed} ms`);
    release();
    console.log('PASS unavailable persistence cannot extend the configured Worker drain deadline');
  } finally {
    release?.();
    if (tenantId) await tenantStatus('active');
    await stopWorker();
    proxy.closeAllConnections();
    await new Promise(done => proxy.close(done));
  }
}
