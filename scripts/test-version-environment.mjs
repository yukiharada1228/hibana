// Called by the disposable HTTP harness with real CP, Worker, PostgreSQL and Redis.
import assert from 'node:assert/strict';
import {apiClient, deploy, rollback} from '../sdk/src/api.mjs';

export async function testVersionEnvironment(h) {
  const {api, sql, token, id, wasm, upload, app, holdStorage, releaseStorage, url, root, artifact} = h;
  const base = `/components/${id}`;
  const tenant = (await sql(`SELECT tenant_id FROM components WHERE id='${id}'`)).trim();
  const user = await (await api(`/tenants/${tenant}/users`, {token, method:'POST', body:{email:'deploy@example.invalid',password:'test-password',role:'member'}})).json();
  const minted = await api('/tokens', {token,method:'POST',body:{user_id:user.user_id,scopes:['read','deploy']}});
  assert.equal(minted.status,201);
  const limited = (await minted.json()).token;
  assert.ok(limited);
  const previousUrl = process.env.HIBANA_URL;
  process.env.HIBANA_URL = url;
  const client = await apiClient(root, {token:limited});
  if (previousUrl === undefined) delete process.env.HIBANA_URL; else process.env.HIBANA_URL = previousUrl;
  const config = {name:'upload',vars:{GREETING:'one'},secrets:[],resources:{}};
  await deploy(client, config, artifact, 'atomic-one');
  const expected = (message, secret=false, rotated=false) => ({status:200,body:{message,secret,rotated,unselected:false}});
  assert.deepEqual(await app('upload'),expected('one'));
  assert.equal((await api(`${base}/config`, {token:limited,method:'PUT',body:{env:{GREETING:'mutated'}}})).status,409);
  assert.equal((await api(`${base}/versions/atomic-one/capabilities`, {token,method:'PUT',body:{env:['RESTORE_TOKEN']}})).status,409);
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`, {token:limited,method:'PUT',body:{allowed:true}})).status,403);
  assert.equal((await api(`${base}/secrets/UNSELECTED`, {token,method:'PUT',body:{value:'never-injected'}})).status,201);
  assert.equal((await api(`${base}/secrets/UNSELECTED/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
  const before = (await api(`${base}/config`, {token:limited}));
  assert.deepEqual((await before.json()).env,{GREETING:'one'});
  await assert.rejects(deploy(client,{...config,vars:{GREETING:'denied'},secrets:['RESTORE_TOKEN']},artifact,'denied'), /HTTP 403/);
  assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${id}' AND version='denied'`)).trim(),'0');
  assert.deepEqual(await app('upload'),expected('one'));
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
  await deploy(client,{...config,vars:{GREETING:'two'},secrets:['RESTORE_TOKEN']},artifact,'atomic-two');
  assert.deepEqual(await app('upload'),expected('two',true));
  console.log('PASS Read+Deploy publishes HTTP code/vars; admin-only explicit Secret authorization; unselected Secrets stay absent');

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
