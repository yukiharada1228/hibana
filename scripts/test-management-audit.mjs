// Real HTTP/DB regression, run only by test-http.sh in its disposable database.
import assert from 'node:assert/strict';
import {createHash, generateKeyPairSync} from 'node:crypto';

export async function testManagementAudit({api, sql, wasm, upload}) {
  const created = await api('/admin/tenants', {method:'POST',token:'test-only',body:{
    slug:'management-audit',name:'Management audit',admin_email:'test@example.invalid',admin_password:'test-password',
  }});
  assert.equal(created.status,201);
  const {tenant_id:tenant,admin_user_id:user} = await created.json();
  const login = await api('/auth/login', {method:'POST',body:{tenant_slug:'management-audit',email:'test@example.invalid',password:'test-password'}});
  assert.equal(login.status,201);
  const {token:userToken} = await login.json();
  const snapshot = async () => JSON.parse((await sql(`SELECT json_build_object('status',status,'quotas',quotas) FROM tenants WHERE id='${tenant}'`)).trim());
  const auditActors = async (action,target) => JSON.parse((await sql(`SELECT coalesce(json_agg(actor ORDER BY id),'[]') FROM audit_logs
    WHERE tenant_id='${tenant}' AND action='${action}' AND ${target === null ? 'target IS NULL' : `target='${target}'`}`)).trim());
  const settings = [
    {path:'status',action:'tenant_status_updated',body:{status:'suspended'}},
    {path:'quotas',action:'tenant_quotas_updated',body:{max_concurrent_executions:1}},
  ];
  const before = await snapshot();
  for (const {path,action,body} of settings) {
    const endpoint = `/admin/tenants/${tenant}/${path}`;
    assert.equal((await api(endpoint,{token:userToken,method:'PUT',body})).status,401,'tenant admin cannot change platform settings');
    assert.equal((await api(`/admin/tenants/ten_missing/${path}`,{token:'test-only',method:'PUT',body})).status,404);
    // Force the audit INSERT to fail after the setting UPDATE has executed.
    await sql(`ALTER TABLE audit_logs ADD CONSTRAINT test_reject_settings_audit CHECK (action <> '${action}') NOT VALID`);
    try {
      assert.equal((await api(endpoint,{token:'test-only',method:'PUT',body})).status,500);
      assert.deepEqual(await snapshot(),before,'failed audit must roll back the setting');
      assert.deepEqual(await auditActors(action,tenant),[]);
    } finally {
      await sql('ALTER TABLE audit_logs DROP CONSTRAINT test_reject_settings_audit');
    }
  }
  for (const {path,action,body} of settings) {
    assert.equal((await api(`/admin/tenants/${tenant}/${path}`,{token:'test-only',method:'PUT',body})).status,200);
    assert.deepEqual(await auditActors(action,tenant),['bootstrap']);
  }
  assert.deepEqual(await snapshot(),{status:'suspended',quotas:{max_concurrent_executions:1}});
  assert.equal((await api(`/admin/tenants/${tenant}/status`,{token:'test-only',method:'PUT',body:{status:'active'}})).status,200);
  assert.equal((await api(`/admin/tenants/${tenant}/quotas`,{token:'test-only',method:'PUT',body:{}})).status,200);
  assert.deepEqual(await snapshot(),before);
  for (const {action} of settings) assert.deepEqual(await auditActors(action,tenant),['bootstrap','bootstrap']);
  console.log('PASS tenant settings: audit failure rolls back changes; successful updates record bootstrap; tenant admin and missing-tenant requests leave no success audit');

  const serviceToken = 'disposable-management-audit-service', serviceId = 'tok_management_audit';
  const hash = createHash('sha256').update(serviceToken).digest('hex');
  await sql(`INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at) VALUES
    ('${serviceId}','${tenant}',NULL,'${hash}',ARRAY['read','admin','deploy'],now()+interval '10 minutes')`);
  for (const [kind,token,actor] of [['user',userToken,user],['service',serviceToken,serviceId]]) {
    // Check exact attribution for each operation, including actions with NULL targets.
    const audited = async (action,target,operation,status=200) => {
      const previous = await auditActors(action,target);
      const response = await operation();
      assert.equal(response.status,status,`${kind}: ${action}`);
      assert.deepEqual(await auditActors(action,target),[...previous,actor],`${kind}: ${action} actor`);
      return response;
    };
    const send = (path,body,method='PUT') => api(path,{token,method,body});
    const component = await send('/components',{name:`audit-${kind}`},'POST');
    assert.equal(component.status,201);
    const id = (await component.json()).component_id, base = `/components/${id}`;
    await audited('secret_updated',id,() => send(`${base}/secrets/TOKEN`,{value:'fixture-only'}),201);
    await audited('secret_rotated',id,() => send(`${base}/secrets/TOKEN/rotate`,{value:'rotated-fixture'},'POST'));
    await audited('secret_deploy_access_changed',id,() => send(`${base}/secrets/TOKEN/deploy-access`,{allowed:true}));
    await audited('secret_deleted',id,() => send(`${base}/secrets/TOKEN`,undefined,'DELETE'),204);

    const {publicKey} = generateKeyPairSync('ed25519');
    const public_key = publicKey.export({type:'spki',format:'der'}).subarray(-32).toString('base64url');
    const keyId = `audit-${kind}`;
    await audited('signing_key_registered',keyId,() => send(`/admin/signing-keys/${keyId}`,{public_key}));
    await audited('signing_policy_updated',null,() => send('/admin/signing-policy',{require_signed_components:true}));
    try {
      await audited('component_signature_rejected',id,() => upload(id,token,'unsigned',wasm),400);
    } finally {
      await audited('signing_policy_updated',null,() => send('/admin/signing-policy',{require_signed_components:false}));
    }
    await audited('signing_key_retired',keyId,() => send(`/admin/signing-keys/${keyId}`,undefined,'DELETE'),204);

    const first = await audited('version_environment_published',id,() => upload(id,token,'one',wasm),201);
    await audited('capability_egress_approved',first.data.version_id,() => send(`${base}/versions/one/capabilities/egress`,{allow_outbound:['example.invalid:443']}));
    await audited('component_egress_updated',id,() => send(`${base}/egress`,{allow:['example.invalid:443']},'PATCH'));
    await audited('version_environment_published',id,() => upload(id,token,'two',wasm),201);
    await audited('active_version_switched',id,() => send(`${base}/active-version`,{version:'one'}));
    await audited('version_rollback',id,() => send(`${base}/rollback`,{version:'two'},'POST'));
    assert.equal((await send(base,undefined,'DELETE')).status,204);
    console.log(`PASS ${kind} audit attribution: Secrets, signing keys/policy/rejection, egress, deployment, version switching and rollback`);
  }
}
