// Real HTTP/ORM/Worker checks against test-http.sh's disposable database.
import assert from 'node:assert/strict';
import {request} from 'node:http';

export async function testEnvironmentLimits({api, sql, token, wasm, upload, url}) {
  const created = await api('/components', {token, method:'POST', body:{name:'environment-limits'}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  const base = `/components/${id}`;
  const secret = (value, rotate=false) => api(`${base}/secrets/Z_TOKEN${rotate ? '/rotate' : ''}`, {
    token, method:rotate ? 'POST' : 'PUT', body:{value},
  });
  const invoke = () => new Promise((done, reject) => {
    const req = request(url+'/env-size', {headers:{Host:'environment-limits.upload.hibana.test'}}, res => {
      let body = '';
      res.on('data', chunk => body += chunk);
      res.on('error', reject);
      res.on('end', () => done({status:res.statusCode, body}));
    });
    req.setTimeout(10000, () => req.destroy(new Error('Environment test deadline')));
    req.on('error', reject); req.end();
  });
  assert.equal((await secret('s'.repeat(1024))).status,201);
  assert.equal((await api(`${base}/secrets/Z_TOKEN/deploy-access`, {token, method:'PUT', body:{allowed:true}})).status,200);
  assert.equal((await upload(id,token,'initial',wasm,0,{secrets:['Z_TOKEN'],ingress:true})).status,201);
  const snapshot = () => sql(`SELECT json_build_object('active',active_version_id,'previous',previous_active_version_id) FROM components WHERE id='${id}'`);
  const before = await snapshot();
  const vars = Object.fromEntries(Array.from({length:8}, (_, i) => [`A${i}`, 'あ'.repeat(1333)]));
  const bytes = Object.entries(vars).reduce((n,[key,value]) => n+Buffer.byteLength(key)+Buffer.byteLength(value),0);
  const remaining = 32768-bytes-Buffer.byteLength('Z_TOKEN');
  const oversized = await upload(id,token,'oversized',wasm,0,{vars,secrets:['Z_TOKEN'],ingress:true});
  assert.equal(oversized.status,400,'combined vars and Secret bytes must be checked before publication');
  assert.equal(await snapshot(),before);
  assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${id}' AND version='oversized'`)).trim(),'0');
  assert.equal(JSON.parse((await invoke()).body).secret_bytes,1024);
  assert.equal((await secret('s'.repeat(remaining))).status,200);
  const boundary = await upload(id,token,'boundary',wasm,0,{vars,secrets:['Z_TOKEN'],ingress:true});
  assert.equal(boundary.status,201,'the exact byte limit must remain usable');
  assert.deepEqual(JSON.parse((await invoke()).body),{secret_bytes:remaining,var_bytes:3999});
  const generation = () => sql(`SELECT current_version FROM function_secrets WHERE component_id='${id}' AND name='Z_TOKEN' AND deleted_at IS NULL`);
  const ledger = () => sql(`SELECT count(*) FROM function_secret_versions v JOIN function_secrets s ON s.id=v.secret_id AND s.tenant_id=v.tenant_id WHERE s.component_id='${id}'`);
  const originalGeneration = await generation(), originalLedger = await ledger();
  for (const rotate of [false,true]) {
    assert.equal((await secret('s'.repeat(remaining+1),rotate)).status,409);
    assert.equal(await generation(),originalGeneration);
    assert.equal(await ledger(),originalLedger,'a refused update must not append a Secret generation');
    assert.deepEqual(JSON.parse((await invoke()).body),{secret_bytes:remaining,var_bytes:3999});
  }
  // Stored, unselected Secrets do not consume this version's environment budget.
  assert.equal((await api(`${base}/secrets/UNSELECTED`, {token,method:'PUT',body:{value:'u'.repeat(4096)}})).status,201);
  assert.equal((await upload(id,token,'small',wasm,0,{vars:{GREETING:'small'}})).status,201);
  assert.equal((await secret('s'.repeat(remaining+1))).status,409,'inactive rollback versions remain protected');
  assert.equal((await upload(id,token,'smaller',wasm,0,{vars:{GREETING:'smaller'}})).status,201);
  assert.equal((await api(`${base}/versions/by-id/${boundary.data.version_id}`, {token,method:'DELETE'})).status,204);
  assert.equal((await secret('s'.repeat(4096))).status,200,'deleted versions must not prevent rotation');

  // Legacy/direct-DB data must fail before the guest, never run with partial env.
  assert.equal((await secret('s'.repeat(remaining))).status,200);
  const legacy = await upload(id,token,'legacy',wasm,0,{vars,secrets:['Z_TOKEN']});
  assert.equal(legacy.status,201);
  await sql(`UPDATE version_configs SET value=repeat('x',4096) WHERE tenant_id=(SELECT tenant_id FROM components WHERE id='${id}') AND version_id='${legacy.data.version_id}' AND key='A0'`);
  assert.equal((await invoke()).status,502);
  const execution = JSON.parse((await sql(`SELECT json_build_object('status',status,'error',error) FROM executions WHERE component_id='${id}' ORDER BY created_at DESC LIMIT 1`)).trim());
  assert.equal(execution.status,'failed');
  assert.match(JSON.stringify(execution.error),/Vars and Secrets together exceed the environment size limit/);
  await sql(`UPDATE version_configs SET value=repeat('あ',1333) WHERE tenant_id=(SELECT tenant_id FROM components WHERE id='${id}') AND version_id='${legacy.data.version_id}' AND key='A0'`);
  assert.deepEqual(JSON.parse((await invoke()).body),{secret_bytes:remaining,var_bytes:3999});
  assert.equal((await api(base,{token,method:'DELETE'})).status,204);
  console.log('PASS combined UTF-8 environment limits; atomic failed deploy/rotation; inactive-version protection; deleted/unselected exclusion; legacy Worker rejection');
}
