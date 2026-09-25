// Real HTTP regressions, run only by test-http.sh with its disposable database.
import assert from 'node:assert/strict';
import {request} from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';

export async function testIngressReadiness({api, sql, token, wasm, upload, url, operator,
  holdDownloads, startWorker, stopWorker, metricsUrl, cacheDir, restart, workerPort}) {
  const created = await api('/components', {method:'POST', token, body:{name:'readiness'}});
  assert.equal(created.status,201);
  const {component_id:id} = await created.json();
  assert.equal((await upload(id,token,'1',wasm,0,{ingress:true})).status,201);
  const invoke = (name='readiness') => new Promise((done,reject) => {
    const req = request(url+'/', {headers:{Host:`${name}.upload.hibana.test`}}, res => {
      let body=''; res.on('data', chunk => body+=chunk);
      res.on('end', () => done({status:res.statusCode,body})); res.on('error',reject);
    });
    req.setTimeout(10000, () => req.destroy(new Error('Readiness fixture deadline')));
    req.on('error',reject); req.end();
  });
  assert.equal((await invoke()).status,200);
  assert.equal((await invoke('missing')).status,404);
  const before = await sql(`SELECT count(*) FROM executions WHERE component_id='${id}'`);
  for (const privilege of ['EXECUTE ON FUNCTION public.auth_lookup_tenant_id_by_slug(text)', 'SELECT ON public.components']) {
    await sql(`REVOKE ${privilege} FROM faas_app`);
    try {
      const failure = await invoke();
      assert.equal(failure.status,503, 'lookup failure must not be classified as a missing app');
      assert.deepEqual(JSON.parse(failure.body), {error:{code:'unavailable',message:'internal server error',retryable:true}});
    } finally { await sql(`GRANT ${privilege} TO faas_app`); }
  }
  assert.equal(await sql(`SELECT count(*) FROM executions WHERE component_id='${id}'`),before,
    'failed lookup must not accept an execution');
  assert.equal((await invoke()).status,200);
  console.log('PASS ingress tenant/component lookup failures return retryable 503; missing apps remain 404 and recovery serves the app');

  const readiness = () => fetch(metricsUrl+'/readyz', {signal:AbortSignal.timeout(1000)});
  const release = holdDownloads();
  let cold;
  try {
    await stopWorker();
    await restart({});
    await startWorker(undefined,{WASM_CACHE_DIR:cacheDir});
    for (let n=0; n<3; n++) {
      assert.equal((await fetch(metricsUrl+'/healthz')).status,200);
      assert.equal((await readiness()).status,200, 'empty cache must not prevent admission');
      await sleep(200);
    }
    const metrics = await (await fetch(metricsUrl+'/metrics')).text();
    assert.match(metrics, /^wasmtime_component_cache_misses_total 0$/m, 'no background fleet-wide warming');
    const count = await sql(`SELECT count(*) FROM executions WHERE component_id='${id}'`);
    cold = invoke(); cold.catch(() => {});
    for (let n=0;n<50;n++) {
      if (await sql(`SELECT count(*) FROM executions WHERE component_id='${id}'`) !== count) break;
      await sleep(50);
    }
    assert.equal((await sql(`SELECT status FROM executions WHERE component_id='${id}' ORDER BY created_at DESC LIMIT 1`)).trim(),'pending',
      'a blocked cold load must not claim a guest');
    assert.equal((await readiness()).status,200);
  } finally { release(); }
  assert.equal((await cold).status,200, 'same cold request executes after recovery without client retry');
  // Cancellation while storage is stalled must be rechecked after restoration.
  const releaseCanceled = holdDownloads();
  let canceled;
  try {
    await stopWorker();
    await startWorker(undefined,{WASM_CACHE_DIR:cacheDir+'-canceled'});
    canceled = invoke(); canceled.catch(() => {});
    let pending;
    for (let n=0;n<100;n++) {
      pending = (await sql(`SELECT id FROM executions WHERE component_id='${id}' AND status='pending'`)).trim();
      if (pending) break;
      await sleep(50);
    }
    assert.ok(pending);
    let loading=false;
    for (let n=0;n<100;n++) {
      loading=/^wasmtime_component_cache_misses_total 1$/m.test(await (await fetch(metricsUrl+'/metrics')).text());
      if (loading) break;
      await sleep(50);
    }
    assert.ok(loading, 'download must have started before cancellation');
    await sql(`UPDATE executions SET status='failed',finished_at=now() WHERE id='${pending}'`);
  } finally { releaseCanceled(); }
  assert.equal((await canceled).status,401);
  assert.doesNotMatch(await (await fetch(metricsUrl+'/metrics')).text(), /^executions_total\{outcome="succeeded"\} [1-9]/m);
  console.log('PASS cancellation during cold restoration never starts the guest');
  await operator('prepare','','127.0.0.1');
  assert.equal((await invoke()).status,200);
  assert.equal((await api('/components',{token})).status,200);
  console.log('PASS empty Worker is ready without fleet-wide warming; cold request waits before claim, then serves HTTP');
}
