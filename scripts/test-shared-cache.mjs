// Real Worker processes, the disposable DB and the S3 fixture from test-http.sh.
import assert from 'node:assert/strict';
import {createPrivateKey, createHash, sign} from 'node:crypto';
import {setTimeout as sleep} from 'node:timers/promises';
import {join} from 'node:path';

export async function testSharedCache({api, sql, token, wasm, upload, app, internal,
  restart, startWorker, stopWorker, metricsUrl, folder, objects, objectSizes, failWrites}) {
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
  assert.equal(cacheKeys.length,257, 'published artifact is outside the 256 spare-entry budget');
  assert.ok(objects.has('/test-components/'+row.storage_uri), 'GC retains original Wasm');
  console.log('PASS cache upload authorization and MAC binding; source and published native code are retained');

  // More published objects than both the old 256 budget and 4096 scan limit.
  // These are metadata fixtures, never passed to the unsafe deserializer.
  const digest = n => n.toString(16).padStart(64,'0');
  const fixtureKey = n => prefix+'00'.repeat(32)+'/'+digest(n)+'.cwasm';
  for (const key of cacheKeys) if (key !== cacheKey) objects.delete(key);
  await sql(`
    INSERT INTO components(id,tenant_id,name)
      SELECT 'cache-pin-'||n, CASE WHEN n%2=0 THEN 'http' ELSE 'other' END, 'cache-pin-'||n FROM generate_series(1,4097) n;
    INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256)
      SELECT id||'-v1',tenant_id,id,'1','unused',lpad(to_hex(substring(id from 11)::int),64,'0') FROM components WHERE id LIKE 'cache-pin-%';
    UPDATE components SET active_version_id=id||'-v1' WHERE id LIKE 'cache-pin-%';
    INSERT INTO components(id,tenant_id,name) VALUES ('cache-pin-duplicate','http','cache-pin-duplicate');
    INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256)
      VALUES ('cache-pin-duplicate-v1','http','cache-pin-duplicate','1','unused','${digest(1)}');
    UPDATE components SET active_version_id=id||'-v1' WHERE id='cache-pin-duplicate';
    INSERT INTO component_versions(id,tenant_id,component_id,version,storage_uri,wasm_sha256)
      SELECT 'cache-history-'||n,'other','cache-pin-1',n::text,'unused',lpad(to_hex(n),64,'0') FROM generate_series(10001,10003) n;
    UPDATE components SET previous_active_version_id='cache-history-10001' WHERE id='cache-pin-1';
    INSERT INTO executions(id,tenant_id,component_id,version_id,status,http_request)
      VALUES ('cache-running','other','cache-pin-1','cache-history-10002','running',true),
             ('cache-pending','other','cache-pin-1','cache-history-10003','pending',true);
    INSERT INTO artifact_reservations(id,tenant_id,version_id,storage_uri,wasm_sha256,expires_at)
      VALUES ('cache-reserved','other','not-yet-published','unused','${digest(10004)}',now()+interval '5 minutes'),
             ('cache-expired','other','abandoned','unused','${digest(10005)}',now()-interval '1 second');
  `);
  const pinned = [...Array.from({length:4097}, (_,i)=>i+1),10001,10002,10003,10004];
  for (const n of [...pinned,10005,...Array.from({length:300}, (_,i)=>20000+i)]) objects.set(fixtureKey(n),Buffer.from('metadata fixture'));
  // Advertise real-world native sizes without allocating hundreds of GiB.
  // Protected objects alone exceed 2 GiB; they must not consume the spare budget.
  for (const n of pinned) objectSizes.set(fixtureKey(n),55*1024*1024);
  const publish = async () => {
    const result = await fetch(internal+'/internal/compiled-artifact',{method:'POST',headers,body:original});
    assert.equal(result.status,204,await result.text());
  };
  const beforeFailedScan = [...objects.keys()].sort();
  await sql('REVOKE EXECUTE ON FUNCTION hibana_protected_artifact_hashes() FROM faas_app');
  try {
    const failed = await fetch(internal+'/internal/compiled-artifact',{method:'POST',headers,body:original});
    assert.equal(failed.status,503,await failed.text());
    assert.deepEqual([...objects.keys()].sort(),beforeFailedScan,'a failed reference lookup must not delete any objects');
  } finally {
    await sql('GRANT EXECUTE ON FUNCTION hibana_protected_artifact_hashes() TO faas_app');
  }
  await publish();
  for (const n of pinned) assert.ok(objects.has(fixtureKey(n)),`protected native artifact ${n} must survive GC`);
  assert.equal([...objects.keys()].filter(k=>k.startsWith(prefix)).length,pinned.length+1+256);
  assert.ok(!objects.has(fixtureKey(10005)), 'expired preparation is reclaimable');
  // A deleted app and a released rollback/execution pin become reclaimable.
  await sql(`UPDATE components SET deleted_at=now() WHERE id='cache-pin-1';
    UPDATE executions SET status='succeeded' WHERE id IN ('cache-running','cache-pending');
    DELETE FROM artifact_reservations WHERE id IN ('cache-reserved','cache-expired');`);
  await publish();
  for (const n of [10001,10002,10003,10004]) assert.ok(!objects.has(fixtureKey(n)),`released artifact ${n} should be collected`);
  assert.ok(objects.has(fixtureKey(1)),'another tenant still publishes the same source hash');
  await sql("UPDATE components SET deleted_at=now() WHERE id='cache-pin-duplicate'");
  await publish();
  assert.ok(!objects.has(fixtureKey(1)),'last live reference released');
  for (const key of [...objects.keys()]) if (key.startsWith(prefix) && key !== cacheKey) objects.delete(key);
  objectSizes.clear();
  await sql(`UPDATE components SET deleted_at=now() WHERE id LIKE 'cache-pin-%'`);
  console.log('PASS 4097 published apps across tenants, rollback targets, pending/running executions and deployment pins survive shared GC; released references are collected');

  // Reactivating a historical version with only a local copy must republish it.
  const beforeRepublish = await metrics();
  objects.delete(cacheKey);
  assert.equal((await upload(id,token,'shared-local-republish',wasm)).status,201);
  assert.ok(objects.has(cacheKey));
  const misses = text => text.match(/^wasmtime_component_cache_misses_total (\d+)$/m)?.[1];
  assert.equal(misses(await metrics()),misses(beforeRepublish),'local preparation republishes without recompilation');
  await replace();
  assert.equal((await app('shared-cache')).status,200);
  assert.match(await metrics(), /^wasmtime_component_cache_misses_total 0$/m);
  assert.match(await metrics(), /^hibana_worker_shared_cache_total\{outcome="hit"\} 1$/m);
  console.log('PASS warm publication restores a missing shared copy; the following empty Pod reuses it without compilation');

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
