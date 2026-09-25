// Real Worker processes, the disposable DB and the S3 fixture from test-http.sh.
import assert from 'node:assert/strict';
import {createPrivateKey, createHash, sign} from 'node:crypto';
import {setTimeout as sleep} from 'node:timers/promises';
import {join} from 'node:path';

export async function testSharedCache({api, sql, token, wasm, upload, app, internal,
  restart, startWorker, stopWorker, metricsUrl, folder, objects, failWrites}) {
  const key = 'ba'.repeat(32);
  const env = {COMPILED_CACHE_KEY:key};
  const prefix = '/test-components/_hibana/compiled/v1/';
  // Prior test applications are retained in this disposable database but are no
  // longer active. Each cold invocation loads exactly this fixture's source hash.
  await stopWorker();
  await sql('UPDATE components SET active_version_id=NULL');
  await restart(env);
  let generation = 0;
  const launch = async (extra = {}) => {
    await startWorker(internal, {...env, WASM_CACHE_DIR:join(folder, `shared-cache-${generation++}`), ...extra});
  };
  const metrics = async () => (await fetch(metricsUrl+'/metrics')).text();
  const waitReady = async () => {
    for (let i=0; i<200; i++) {
      if ((await fetch(metricsUrl+'/readyz')).ok) return;
      await sleep(100);
    }
    assert.fail('Worker infrastructure did not become ready');
  };
  await launch();
  const created = await api('/components', {token, method:'POST', body:{name:'shared-cache'}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  assert.equal((await upload(id, token, 'shared-1', wasm, 0, {ingress:true})).status,201);
  assert.equal((await app('shared-cache')).status,200);
  let cacheKeys = [...objects.keys()].filter(k => k.startsWith(prefix));
  assert.equal(cacheKeys.length,1, 'first compilation publishes one signed artifact');
  assert.match(await metrics(), /^hibana_worker_shared_cache_total\{outcome="stored"\} 1$/m);
  const cacheKey = cacheKeys[0], original = Buffer.from(objects.get(cacheKey));
  const replace = async () => { await stopWorker(); await launch(); await waitReady(); };
  await replace();
  assert.equal((await app('shared-cache')).status,200);
  assert.match(await metrics(), /^wasmtime_component_cache_misses_total 0$/m);
  assert.match(await metrics(), /^hibana_worker_shared_cache_total\{outcome="hit"\} 1$/m);
  assert.equal((await app('shared-cache')).status,200);
  console.log('PASS empty replacement Worker restores signed native code: zero compilations, real HTTP 200');

  const corrupt = Buffer.from(original); corrupt[0] ^= 1;
  objects.set(cacheKey,corrupt);
  await replace();
  assert.equal((await app('shared-cache')).status,200);
  assert.match(await metrics(), /^hibana_worker_shared_cache_total\{outcome="invalid"\} 1$/m);
  assert.match(await metrics(), /^wasmtime_component_cache_misses_total 1$/m);
  assert.equal((await app('shared-cache')).status,200);
  assert.notDeepEqual(objects.get(cacheKey),corrupt);
  console.log('PASS corrupt shared native code is rejected, recompiled and repaired without executing it');

  // The upload API requires both an authorized preparation and native-code MAC.
  const [runtime, source] = cacheKey.slice(prefix.length).replace(/\.cwasm$/, '').split('/');
  assert.equal(source,createHash('sha256').update(wasm).digest('hex'));
  const row = JSON.parse((await sql(`SELECT json_build_object('tenant_id',v.tenant_id,'storage_uri',v.storage_uri) FROM component_versions v WHERE v.component_id='${id}' LIMIT 1`)).trim());
  const now = Math.floor(Date.now()/1000);
  const claims = {...row,sha256:source,kid:process.env.JOB_SIGNING_KID,iat:now,exp:now+120};
  const u64 = n => { const b=Buffer.alloc(8); b.writeBigInt64BE(BigInt(n)); return b; };
  const strings = [claims.tenant_id,claims.storage_uri,claims.sha256,claims.kid].flatMap(s => [u64(Buffer.byteLength(s)),Buffer.from(s)]);
  const payload = Buffer.concat([Buffer.from('hibana-preparation-v1\0'),...strings,u64(now),u64(now+120)]);
  const signing = createPrivateKey({key:Buffer.concat([Buffer.from('302e020100300506032b657004220420','hex'),Buffer.from(process.env.JOB_SIGNING_KEY,'hex')]),format:'der',type:'pkcs8'});
  const preparation = Buffer.from(JSON.stringify(claims)).toString('base64url')+'.'+sign(null,payload,signing).toString('base64url');
  const headers = {'x-hibana-preparation-token':preparation,'x-hibana-cache-runtime':runtime};
  for (const h of [{},headers,{...headers,'x-hibana-cache-runtime':'00'.repeat(32)}]) {
    const res = await fetch(internal+'/internal/compiled-artifact',{method:'POST',headers:h,body:corrupt});
    assert.equal(res.status,401); await res.text();
  }
  // Reuse the valid artifact while forcing entry eviction. Source objects survive.
  for (let i=0;i<256;i++) objects.set(prefix+'00'.repeat(32)+'/'+i.toString(16).padStart(64,'0')+'.cwasm',Buffer.from('old'));
  const accepted = await fetch(internal+'/internal/compiled-artifact',{method:'POST',headers,body:original});
  assert.equal(accepted.status,204); await accepted.text();
  cacheKeys = [...objects.keys()].filter(k=>k.startsWith(prefix));
  assert.equal(cacheKeys.length,256);
  assert.ok(objects.has('/test-components/'+row.storage_uri), 'GC retains original Wasm');
  console.log('PASS cache upload authorization and MAC binding; shared entry eviction preserves source Wasm');

  // A cache smaller than one compiled artifact still serves verified code.
  await stopWorker();
  await launch({WORKER_CACHE_DISK_MIB:'1'}); await waitReady();
  assert.equal((await app('shared-cache')).status,200);
  assert.equal((await app('shared-cache')).status,200);
  assert.match(await metrics(), /^wasmtime_component_cache_misses_total 0$/m);
  assert.match(await metrics(), /^hibana_worker_shared_cache_total\{outcome="hit"\} 1$/m);
  console.log('PASS artifact larger than local disk budget serves HTTP from verified memory and reuses it');

  // A failed shared publication must not reject an otherwise valid deployment.
  await stopWorker();
  failWrites(true);
  await restart(env);
  objects.delete(cacheKey);
  await launch(); await waitReady();
  assert.equal((await app('shared-cache')).status,200);
  for (let i=0;i<100 && !(await metrics()).includes('outcome="store_failed"');i++) await sleep(50);
  assert.match(await metrics(), /^hibana_worker_shared_cache_total\{outcome="store_failed"\} 1$/m);
  failWrites(false);
  console.log('PASS unavailable shared publication does not prevent local preparation or HTTP execution');
}
