// Real HTTP/DB regression, run only against test-http.sh's disposable database.
import assert from 'node:assert/strict';
import {issueFixtureToken} from './test-api-credentials.mjs';

export async function testSecretMetadata({api, sql}) {
  const fixtures = [];
  for (const slug of ['secret-metadata', 'secret-metadata-other']) {
    const created = await api('/admin/tenants', {token:'test-only', method:'POST', body:{
      slug, name:slug, admin_email:'test@example.invalid', admin_oidc_subject:'fixture-admin',
    }});
    assert.equal(created.status,201);
    const {tenant_id:tenant} = await created.json();
    const {token} = await (await issueFixtureToken(sql, {tenant_slug:slug, email:'test@example.invalid'})).json();
    const component = await api('/components', {token, method:'POST', body:{name:'metadata'}});
    assert.equal(component.status,201);
    const {component_id:id} = await component.json();
    const base = `/components/${id}/secrets`;
    const write = (name, value, rotate=false) => api(`${base}/${name}${rotate ? '/rotate' : ''}`, {
      token, method:rotate ? 'POST' : 'PUT', body:{value},
    });
    assert.equal((await write('MISSING','fixture',true)).status,404);
    assert.equal((await write('TOKEN','old')).status,201);
    assert.equal((await write('TOKEN','second')).status,200);
    assert.equal((await write('TOKEN','latest',true)).status,200);
    assert.equal((await write('REUSED','old')).status,201);
    assert.equal((await api(`${base}/REUSED`, {token, method:'DELETE'})).status,204);
    assert.equal((await write('REUSED','new')).status,201);
    assert.equal((await write('DELETED','gone')).status,201);
    assert.equal((await api(`${base}/DELETED`, {token, method:'DELETE'})).status,204);
    fixtures.push({tenant, token, id, base, slug});
  }
  const [first, other] = fixtures;
  const {token:reader} = await (await issueFixtureToken(sql, {
    tenant_slug:first.slug, email:'test@example.invalid', scopes:['read'],
  })).json();
  assert.equal((await api(`${first.base}/keys`, {token:reader})).status,403);
  assert.equal((await api(`${other.base}/keys`, {token:first.token})).status,404);

  // Metadata endpoints must work without permission to read encrypted material.
  // Restore the table grant even if an assertion fails; this is a disposable DB.
  const columns = 'tenant_id,secret_id,version,kek_kid,value_len';
  await sql(`REVOKE SELECT ON function_secret_versions FROM faas_app;
    GRANT SELECT (${columns}) ON function_secret_versions TO faas_app`);
  try {
    for (const {token, base} of fixtures) {
      const response = await api(`${base}/keys`, {token});
      assert.equal(response.status,200,'key inventory must not select ciphertext, nonces or wrapped keys');
      assert.deepEqual(await response.json(), {secrets:[
        {name:'REUSED', version:1, kek_kid:'rotated-test', value_len:3},
        {name:'TOKEN', version:3, kek_kid:'rotated-test', value_len:6},
      ]});
    }
    const response = await api(first.base, {token:reader});
    assert.equal(response.status,200);
    const {secrets} = await response.json();
    assert.deepEqual(secrets.map(({name, version, has_value}) => ({name, version, has_value})), [
      {name:'REUSED', version:1, has_value:true}, {name:'TOKEN', version:3, has_value:true},
    ]);
    for (const secret of secrets) {
      assert.deepEqual(Object.keys(secret).sort(), ['has_value','name','updated_at','version']);
    }
  } finally {
    await sql(`GRANT SELECT ON function_secret_versions TO faas_app;
      REVOKE SELECT (${columns}) ON function_secret_versions FROM faas_app`);
  }
  const generations = JSON.parse((await sql(`SELECT json_agg(v.reason ORDER BY v.version)
    FROM function_secret_versions v JOIN function_secrets s ON s.tenant_id=v.tenant_id AND s.id=v.secret_id
    WHERE s.component_id='${first.id}' AND s.name='TOKEN'`)).trim());
  assert.deepEqual(generations,['create','rotate','rotate'],'PUT and rotate must retain the ledger reasons');

  // A full generation counter must reject before encryption, rewrapping or audit.
  const secretId = (await sql(`SELECT id FROM function_secrets WHERE component_id='${first.id}' AND name='TOKEN'`)).trim();
  await sql(`INSERT INTO function_secret_versions
    (tenant_id,secret_id,version,kek_kid,wrapped_dek,dek_nonce,nonce,ciphertext,value_len,reason)
    SELECT tenant_id,secret_id,2147483647,'retired-fixture',wrapped_dek,dek_nonce,nonce,ciphertext,value_len,'rekey'
    FROM function_secret_versions WHERE secret_id='${secretId}' AND version=3;
    UPDATE function_secrets SET current_version=2147483647 WHERE id='${secretId}'`);
  const snapshot = () => sql(`SELECT json_build_object('version',s.current_version,
    'generations',(SELECT count(*) FROM function_secret_versions WHERE secret_id=s.id),
    'audit',(SELECT count(*) FROM audit_logs WHERE tenant_id=s.tenant_id))
    FROM function_secrets s WHERE s.id='${secretId}'`);
  try {
    const before = await snapshot();
    for (const [path,method,body] of [
      [`${first.base}/TOKEN`,'PUT',{value:'blocked'}],
      [`${first.base}/TOKEN/rotate`,'POST',{value:'blocked'}],
      ['/admin/secrets/rekey','POST',undefined],
    ]) {
      assert.equal((await api(path,{token:first.token,method,body})).status,409,'full generation counter must not panic or wrap');
      assert.equal(await snapshot(),before,'rejected updates must not change metadata, ledger or audit');
    }
  } finally {
    await sql(`UPDATE function_secrets SET current_version=3 WHERE id='${secretId}';
      DELETE FROM function_secret_versions WHERE secret_id='${secretId}' AND version=2147483647`);
  }
  console.log('PASS Secret metadata: no encrypted columns required; current generation, reused/deleted names, tenant isolation and Read/Admin scopes');
  console.log('PASS Secret updates: create/rotate reasons preserved; exhausted generation rejects PUT/rotate/rekey without writes or success audits');
}
