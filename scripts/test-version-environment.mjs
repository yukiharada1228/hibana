// Called by the disposable HTTP harness with real CP, Worker, PostgreSQL and Redis.
import assert from 'node:assert/strict';
import {apiClient, deploy, rollback} from '../sdk/src/api.mjs';

export async function testVersionEnvironment(h) {
  const {api, sql, token, id, wasm, upload, app, holdStorage, releaseStorage, url, root, artifact} = h;
  const base = `/components/${id}`;
  const secretBindings = async () => {
    const response = await api(`${base}/config`, {token});
    assert.equal(response.status,200);
    return (await response.json()).secrets;
  };
  const tenant = (await sql(`SELECT tenant_id FROM components WHERE id='${id}'`)).trim();
  const user = await (await api(`/tenants/${tenant}/users`, {token, method:'POST', body:{email:'deploy@example.invalid',oidc_subject:'fixture-deployer',role:'member'}})).json();
  const minted = await api('/tokens', {token,method:'POST',body:{user_id:user.user_id,scopes:['read','deploy']}});
  assert.equal(minted.status,201);
  const limited = (await minted.json()).token;
  assert.ok(limited);
  const previousUrl = process.env.HIBANA_URL;
  process.env.HIBANA_URL = url;
  const client = await apiClient({token:limited});
  if (previousUrl === undefined) delete process.env.HIBANA_URL; else process.env.HIBANA_URL = previousUrl;
  const config = {name:'upload',vars:{GREETING:'one'},secrets:[],resources:{}};
  await deploy(client, config, artifact, 'atomic-one');
  const expected = (message, secret=false, rotated=false) => ({status:200,body:{message,secret,rotated,unselected:false}});
  assert.deepEqual(await app('upload'),expected('one'));
  // Only deployment can change vars/bindings; removed write APIs cannot mutate them.
  assert.equal((await api(`${base}/config`, {token:limited,method:'PUT',body:{env:{GREETING:'mutated'}}})).status,405);
  assert.equal((await api(`${base}/config/GREETING`, {token:limited,method:'DELETE'})).status,404);
  assert.equal((await api(`${base}/versions/atomic-one/capabilities`, {token,method:'PUT',body:{env:['RESTORE_TOKEN']}})).status,405);
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`, {token:limited,method:'PUT',body:{allowed:true}})).status,403);
  // Name collisions need only keys, never plaintext vars or their timestamps.
  const columns = 'tenant_id,component_id,version_id,key';
  await sql(`REVOKE SELECT ON version_configs FROM faas_app;
    GRANT SELECT (${columns}) ON version_configs TO faas_app`);
  try {
    assert.equal((await api(`${base}/secrets/GREETING`, {token,method:'PUT',body:{value:'blocked'}})).status,409,
      'an active var blocks the same Secret name without reading its value');
    assert.equal((await api(`${base}/secrets/UNSELECTED`, {token,method:'PUT',body:{value:'never-injected'}})).status,201,
      'an unrelated Secret can be created without reading vars');
  } finally {
    await sql(`GRANT SELECT ON version_configs TO faas_app;
      REVOKE SELECT (${columns}) ON version_configs FROM faas_app`);
  }
  assert.equal((await api(`${base}/secrets/UNSELECTED/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
  const before = (await api(`${base}/config`, {token:limited}));
  assert.deepEqual((await before.json()).env,{GREETING:'one'});
  assert.deepEqual(await secretBindings(),[], 'unselected Secrets are not part of the version');
  await assert.rejects(deploy(client,{...config,vars:{GREETING:'denied'},secrets:['RESTORE_TOKEN']},artifact,'denied'), /HTTP 403/);
  assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${id}' AND version='denied'`)).trim(),'0');
  assert.deepEqual(await app('upload'),expected('one'));
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
  await deploy(client,{...config,vars:{GREETING:'two'},secrets:['RESTORE_TOKEN']},artifact,'atomic-two');
  assert.deepEqual(await app('upload'),expected('two',true));
  assert.deepEqual(await secretBindings(),[{name:'RESTORE_TOKEN',available:true}]);
  console.log('PASS Read+Deploy publishes HTTP code/vars; admin-only explicit Secret authorization; unselected Secrets stay absent');

  // Multiple entries are published together; a rejected batch cannot replace them.
  const batch = await api('/components', {token:limited,method:'POST',body:{name:'batch-environment'}});
  assert.equal(batch.status,201);
  const batchId = (await batch.json()).component_id;
  const batchBase = `/components/${batchId}`;
  for (const name of ['FIRST','SECOND','BLOCKED']) {
    assert.equal((await api(`${batchBase}/secrets/${name}`, {token,method:'PUT',body:{value:`fixture-${name}`}})).status,201);
    if (name !== 'BLOCKED')
      assert.equal((await api(`${batchBase}/secrets/${name}/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
  }
  const batchConfig = async () => {
    const response = await api(`${batchBase}/config`, {token:limited});
    assert.equal(response.status,200);
    return response.json();
  };
  const vars = {GREETING:'batch',EMPTY:'',UNICODE:'雪 🔥'};
  const publishedBatch = await upload(batchId,limited,'batch',wasm,0,{vars,secrets:['FIRST','SECOND']});
  assert.equal(publishedBatch.status,201);
  const savedBatch = await batchConfig();
  assert.equal(savedBatch.version_id,publishedBatch.data.version_id);
  assert.deepEqual(savedBatch.env,vars);
  assert.deepEqual(savedBatch.secrets,[{name:'FIRST',available:true},{name:'SECOND',available:true}]);
  const deniedBatch = await upload(batchId,limited,'batch-denied',wasm,0,{vars:{GREETING:'denied'},secrets:['FIRST','BLOCKED']});
  assert.equal(deniedBatch.status,403);
  assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${batchId}' AND version='batch-denied'`)).trim(),'0');
  assert.deepEqual(await batchConfig(),savedBatch);
  const emptyBatch = await upload(batchId,limited,'batch-empty',wasm,0,{vars:{},secrets:[]});
  assert.equal(emptyBatch.status,201);
  const clearedBatch = await batchConfig();
  assert.equal(clearedBatch.version_id,emptyBatch.data.version_id);
  assert.deepEqual(clearedBatch.env,{});
  assert.deepEqual(clearedBatch.secrets,[]);
  assert.equal(clearedBatch.updated_at,null);
  console.log('PASS multiple vars/Secret bindings, empty batches and rejected publication preserve atomic environment snapshots');

  // Reject after external I/O and after version INSERT. All publication changes roll back.
  const snapshot = () => sql(`SELECT json_build_object('active',active_version_id,'previous',previous_active_version_id,'ingress',ingress_enabled) FROM components WHERE id='${id}'`);
  const stable = await snapshot();
  const waiting = holdStorage();
  const pending = upload(id,limited,'revoked-during-upload',wasm,0,{vars:{GREETING:'mixed'},secrets:['RESTORE_TOKEN'],ingress:false});
  await waiting;
  assert.deepEqual(await app('upload'),expected('two',true));
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`, {token,method:'PUT',body:{allowed:false}})).status,200);
  releaseStorage();
  assert.equal((await pending).status,403);
  assert.equal(await snapshot(),stable);
  assert.deepEqual(await app('upload'),expected('two',true), 'deny-deploy affects future deployments only');
  await rollback(client,config,'atomic-one');
  assert.deepEqual(await app('upload'),expected('one'));
  await rollback(client,config,'atomic-two');
  assert.deepEqual(await app('upload'),expected('two',true));
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN`, {token,method:'PUT',body:{value:'rotated-value'}})).status,200);
  await rollback(client,config,'atomic-one');
  await rollback(client,config,'atomic-two');
  assert.deepEqual(await app('upload'),expected('two',false,true), 'rollback must not rewind Secret values');
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN`, {token,method:'DELETE'})).status,204);
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN`, {token,method:'PUT',body:{value:'restore-test-value'}})).status,201);
  assert.deepEqual(await secretBindings(),[{name:'RESTORE_TOKEN',available:false}], 'availability follows Secret identity, not a recreated name');
  assert.deepEqual(await app('upload'),expected('two'), 'same-name recreated Secret must not bind to an old version');
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
  assert.deepEqual(await app('upload'),expected('two'), 'approval of a new identity cannot change old bindings');
  console.log('PASS failed publication preserves active vars/code/ingress; rollback restores vars without rewinding Secrets; deleted names cannot regain old bindings');

  // Concurrent publication has exactly one winning version and its own environment.
  const concurrent = await Promise.all([
    upload(id,limited,'concurrent-a',wasm,0,{vars:{GREETING:'a'},secrets:['RESTORE_TOKEN'],ingress:true}),
    upload(id,limited,'concurrent-b',wasm,0,{vars:{GREETING:'b'},secrets:[],ingress:true}),
  ]);
  assert.deepEqual(concurrent.map(r=>r.status),[201,201]);
  const active = (await sql(`SELECT v.version FROM components c JOIN component_versions v ON v.id=c.active_version_id WHERE c.id='${id}'`)).trim();
  assert.deepEqual(await app('upload'),expected(active === 'concurrent-a' ? 'a' : 'b', active === 'concurrent-a'));
  const fresh = await (await api('/components',{token:limited,method:'POST',body:{name:'fresh-failure'}})).json();
  assert.equal((await upload(fresh.component_id,limited,'first',wasm,0,{vars:{GREETING:'bad'},secrets:['RESTORE_TOKEN'],ingress:true})).status,403);
  assert.equal((await sql(`SELECT (active_version_id IS NULL AND NOT ingress_enabled)::text FROM components WHERE id='${fresh.component_id}'`)).trim(),'true');
  console.log('PASS concurrent deployments preserve version/config pairs; first failed deployment remains private and inactive');

  // The runtime DB role has RLS even if application code omits a tenant predicate.
  await sql("INSERT INTO version_configs(tenant_id,component_id,version_id,key,value) VALUES ('http','source','source-v1','PRIVATE','other-tenant')");
  const scoped = statement => sql(`BEGIN; SET LOCAL ROLE faas_app; SELECT set_config('app.tenant_id','${tenant}',true); ${statement}; ROLLBACK`);
  assert.equal((await scoped("SELECT count(*) FROM version_configs WHERE tenant_id='http'")).trim().split('\n').at(-1),'0');
  await assert.rejects(scoped(`INSERT INTO version_configs(tenant_id,component_id,version_id,key,value) VALUES ('${tenant}','${fresh.component_id}','${concurrent[0].data.version_id}','WRONG','bad')`));
  const secretId = (await sql(`SELECT id FROM function_secrets WHERE component_id='${id}' AND name='RESTORE_TOKEN' AND deleted_at IS NULL`)).trim();
  await assert.rejects(scoped(`INSERT INTO version_secret_bindings(tenant_id,component_id,version_id,secret_id,name) VALUES ('${tenant}','${id}','${concurrent[1].data.version_id}','${secretId}','WRONG')`));
  await assert.rejects(scoped("INSERT INTO version_configs(tenant_id,component_id,version_id,key,value) VALUES ('http','source','source-v1','FORGED','bad')"));
  await assert.rejects(scoped("UPDATE version_configs SET value='bad'"));
  await assert.rejects(scoped('DELETE FROM version_secret_bindings'));
  console.log('PASS version snapshots enforce tenant/component/Secret identity foreign keys, RLS and append-only runtime privileges');
}
