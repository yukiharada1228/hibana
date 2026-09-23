import { issueFixtureToken } from "./test-api-credentials.mjs";
// Only called by test-http.sh against its disposable database.
import assert from 'node:assert/strict';

export async function testVersionAddressing({api, sql, token, wasm, upload}) {
  const created = await api('/components', {token, method:'POST', body:{name:'version-addressing'}});
  assert.equal(created.status, 201);
  const id = (await created.json()).component_id;
  const base = `/components/${id}/versions`;
  for (const name of ['.', '..', 'a/b', 'a\\b', '%2e', 'a\n', '版1', 'a'.repeat(129)]) {
    assert.equal((await upload(id,token,name,wasm,0,{activate:false})).status, 400);
  }
  assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${id}'`)).trim(), '0');
  const ids = [];
  for (const name of ['v1.2.3-rc.1+build_7', 'a'.repeat(128)]) {
    const result = await upload(id,token,name,wasm,0,{activate:false});
    assert.equal(result.status,201);
    ids.push(result.data.version_id);
    const details = await api(`${base}/${name}`, {token});
    assert.equal(details.status,200);
    assert.equal((await details.json()).version_id,result.data.version_id);
  }
  // Simulate rows accepted by older releases; production data is never rewritten.
  for (const [index,name] of ['.', '..'].entries()) {
    await sql(`UPDATE component_versions SET version='${name}' WHERE id='${ids[index]}'`);
  }
  // A name that looks exactly like another version's ID must remain unambiguous.
  const named = await upload(id,token,ids[0],wasm,0,{activate:false});
  assert.equal(named.status,201);
  assert.equal((await (await api(`${base}/${ids[0]}`,{token})).json()).version_id,named.data.version_id);
  assert.equal((await (await api(`${base}/by-id/${ids[0]}`,{token})).json()).version_id,ids[0]);

  const other = await api('/components',{token,method:'POST',body:{name:'version-other'}});
  const otherId = (await other.json()).component_id;
  const tenant = await api('/admin/tenants',{token:'test-only',method:'POST',body:{slug:'version-outsider',name:'Fixture',admin_email:'fixture@example.invalid',admin_oidc_subject: 'fixture-admin'}});
  assert.equal(tenant.status,201);
  const login = await issueFixtureToken(sql, {tenant_slug:'version-outsider',email:'fixture@example.invalid',});
  assert.equal(login.status,201);
  const outsider = (await login.json()).token;
  for (const method of ['GET','DELETE']) {
    assert.equal((await api(`/components/${otherId}/versions/by-id/${ids[0]}`,{token,method})).status,404);
    assert.equal((await api(`${base}/by-id/${ids[0]}`,{token:outsider,method})).status,404);
    assert.equal((await api(`${base}/by-id/${ids[0]}`,{method})).status,401);
  }
  const reader = await issueFixtureToken(sql, {tenant_slug:'upload',email:'test@example.invalid',scopes:['read']});
  const readToken = (await reader.json()).token;
  assert.equal((await api(`${base}/by-id/${ids[0]}`,{token:readToken})).status,200);
  assert.equal((await api(`${base}/by-id/${ids[0]}`,{token:readToken,method:'DELETE'})).status,403);
  for (const versionId of ids) {
    const path = `${base}/by-id/${versionId}`;
    assert.equal((await api(path,{token,method:'DELETE'})).status,204);
    assert.equal((await api(path,{token})).status,404);
    assert.equal((await api(path,{token,method:'DELETE'})).status,404);
  }
  assert.equal((await api(`${base}/${ids[0]}`,{token})).status,200,'deleting by ID must not delete the lookalike name');
  assert.equal((await api(`/components/${id}`,{token,method:'DELETE'})).status,204);
  assert.equal((await api(`${base}/by-id/${named.data.version_id}`,{token})).status,404,'deleted components hide version details');
  assert.equal((await api(`/components/${otherId}`,{token,method:'DELETE'})).status,204);
  console.log('PASS bounded version labels; legacy dot names work by ID; names/IDs, components, tenants and scopes remain distinct');
}
