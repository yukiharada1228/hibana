// Deletion and its audit row must commit or roll back together.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';

export async function testDeletionAudit({api, sql, token, wasm, upload}) {
  const identity = JSON.parse((await sql(`SELECT json_build_object('tenant',tenant_id,'user',user_id) FROM api_tokens WHERE token_hash='${createHash('sha256').update(token).digest('hex')}'`)).trim());
  const create = async name => {
    const response = await api('/components',{token,method:'POST',body:{name}});
    assert.equal(response.status,201);
    return (await response.json()).component_id;
  };
  const audits = async (action,target) => JSON.parse((await sql(`SELECT coalesce(json_agg(json_build_object('actor',actor,'tenant',tenant_id,'target',target,'detail',detail)), '[]') FROM audit_logs WHERE action='${action}' AND target='${target}'`)).trim());
  const id = await create('deletion-audit');
  const base = `/components/${id}`;
  const active = await upload(id,token,'active',wasm);
  const idle = await upload(id,token,'idle',wasm,0,{activate:false});
  assert.equal(active.status,201); assert.equal(idle.status,201);
  assert.equal((await api(`${base}/versions/by-id/${active.data.version_id}`,{token,method:'DELETE'})).status,409);
  assert.deepEqual(await audits('version_deleted',active.data.version_id),[]);

  // Exercise rollback on an audit INSERT failure, without replacing production code.
  const rejectAudit = async (action,operation) => {
    await sql(`ALTER TABLE audit_logs ADD CONSTRAINT review_reject_deletion CHECK (action <> '${action}') NOT VALID`);
    try { assert.equal((await operation()).status,500); }
    finally { await sql('ALTER TABLE audit_logs DROP CONSTRAINT review_reject_deletion'); }
  };
  const deleteVersion = () => api(`${base}/versions/by-id/${idle.data.version_id}`,{token,method:'DELETE'});
  await rejectAudit('version_deleted',deleteVersion);
  assert.equal((await sql(`SELECT deleted_at IS NULL FROM component_versions WHERE id='${idle.data.version_id}'`)).trim(),'t');
  assert.deepEqual(await audits('version_deleted',idle.data.version_id),[]);
  assert.equal((await deleteVersion()).status,204);
  assert.deepEqual(await audits('version_deleted',idle.data.version_id),[{actor:identity.user,tenant:identity.tenant,target:idle.data.version_id,detail:{component_id:id}}]);
  assert.equal((await deleteVersion()).status,404);
  assert.equal((await audits('version_deleted',idle.data.version_id)).length,1);
  const deleteComponent = () => api(base,{token,method:'DELETE'});
  await rejectAudit('component_deleted',deleteComponent);
  assert.equal((await sql(`SELECT deleted_at IS NULL FROM components WHERE id='${id}'`)).trim(),'t');
  assert.equal((await deleteComponent()).status,204);
  assert.deepEqual(await audits('component_deleted',id),[{actor:identity.user,tenant:identity.tenant,target:id,detail:null}]);
  assert.equal((await deleteComponent()).status,404);
  assert.equal((await audits('component_deleted',id)).length,1);

  const systemId = await create('deletion-audit-system');
  assert.equal((await api(`/admin/tenants/${identity.tenant}/components/${systemId}`,{token:'test-only',method:'DELETE'})).status,204);
  assert.equal((await audits('component_deleted',systemId))[0].actor,'bootstrap');
  // Existing service tokens have no user. Their own ID is the audit actor.
  const serviceId = await create('deletion-audit-service');
  const serviceToken = 'disposable-deletion-audit-service';
  const serviceHash = createHash('sha256').update(serviceToken).digest('hex');
  await sql(`INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at) VALUES ('tok_deletion_audit','${identity.tenant}',NULL,'${serviceHash}',ARRAY['read','admin'],now()+interval '10 minutes')`);
  assert.equal((await api(`/components/${serviceId}`,{token:serviceToken,method:'DELETE'})).status,204);
  assert.equal((await audits('component_deleted',serviceId))[0].actor,'tok_deletion_audit');
  console.log('PASS deletion audit attribution for users, service tokens and bootstrap; denied/repeated deletion has no success audit; audit failure rolls back deletion');
}
