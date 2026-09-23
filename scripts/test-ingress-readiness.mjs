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
  try {
    await stopWorker();
    // An unavailable execution endpoint models the Ready-only Service while
    // preparation discovery can already reach the cold Pod.
    await restart({WORKER_HTTP_URL:'http://127.0.0.1:1', WORKER_PREPARATION_URL:`http://127.0.0.1:${workerPort}`});
    await startWorker(undefined,{WASM_CACHE_DIR:cacheDir});
    for (let n=0; n<3; n++) {
      assert.equal((await fetch(metricsUrl+'/healthz')).status,200);
      assert.equal((await readiness()).status,503, 'cold Worker must not let Kubernetes retire prepared Pods');
      await sleep(400);
    }
  } finally { release(); }
  let ready = false;
  for (let n=0; n<200; n++) {
    if ((await readiness()).status === 200) { ready=true; break; }
    await sleep(200);
  }
  assert.ok(ready, 'separate preparation discovery must warm the Worker before execution discovery can reach it');
  await restart({});
  await operator('prepare','','127.0.0.1');
  assert.equal((await invoke()).status,200);
  assert.equal((await api('/components',{token})).status,200);
  console.log('PASS cold Worker stays alive but unready until active Wasm is prepared; separate preparation discovery and ordinary install barrier recover without closing admission');
}
