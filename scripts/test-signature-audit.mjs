// Real upload rejection must commit an audit without publishing any version.
import assert from 'node:assert/strict';
import {createHash, generateKeyPairSync, sign} from 'node:crypto';

export async function testSignatureAudit({api, sql, token, wasm, upload, url, objects}) {
  const created = await api('/components',{token,method:'POST',body:{name:'signature-audit'}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  const initial = await upload(id,token,'original',wasm);
  assert.equal(initial.status,201);
  const state = () => sql(`SELECT json_build_object('active',active_version_id,'previous',previous_active_version_id,'ingress',ingress_enabled) FROM components WHERE id='${id}'`);
  const before = await state(), objectCount = objects.size;
  assert.equal((await api('/admin/signing-policy',{token,method:'PUT',body:{require_signed_components:true}})).status,200);
  const {publicKey,privateKey} = generateKeyPairSync('ed25519');
  const public_key = publicKey.export({type:'spki',format:'der'}).subarray(-32).toString('base64url');
  const send = async (version,signature) => {
    const form = new FormData();
    form.set('version',version); form.set('activate','true'); form.set('ingress','true');
    form.set('vars',JSON.stringify({GREETING:'must-not-publish-on-rejection'}));
    if (signature !== undefined) form.set('signature',signature);
    form.set('wasm',new Blob([wasm],{type:'application/wasm'}),'component.wasm');
    return fetch(`${url}/components/${id}/versions`,{method:'POST',headers:{Authorization:`Bearer ${token}`},body:form,signal:AbortSignal.timeout(30000)});
  };
  const audit = async version => JSON.parse((await sql(`SELECT coalesce(json_agg(json_build_object('actor',actor,'detail',detail)), '[]') FROM audit_logs WHERE action='component_signature_rejected' AND target='${id}' AND detail->>'version'='${version}'`)).trim());
  try {
    const identity = Buffer.alloc(32); identity[0] = 1;
    for (const [index,bytes] of [Buffer.alloc(32),identity].entries()) {
      const keyId = `audit-weak-${index}`;
      const rejected = await api(`/admin/signing-keys/${keyId}`, {
        token,method:'PUT',body:{public_key:bytes.toString('base64url')},
      });
      assert.equal(rejected.status,400,'keys rejected by strict verification must not be registered');
      await rejected.text();
      assert.equal((await sql(`SELECT count(*) FROM component_signing_keys WHERE key_id='${keyId}'`)).trim(),'0');
      assert.equal((await sql(`SELECT count(*) FROM audit_logs WHERE action='signing_key_registered' AND target='${keyId}'`)).trim(),'0');
    }
    for (const [version,signature,reason] of [
      ['missing',undefined,'signature_required'],
      ['no-key',Buffer.alloc(64).toString('base64url'),'no_signing_keys_registered'],
      ['invalid','invalid-base64!','bad_signature_encoding'],
      ['mismatch',Buffer.alloc(64).toString('base64url'),'no_matching_key'],
    ]) {
      if (version==='invalid') {
        assert.equal((await api('/admin/signing-keys/audit-fixture',{token,method:'PUT',body:{public_key}})).status,200);
      }
      const response = await send(version,signature);
      assert.equal(response.status,400); await response.text();
      const rows = await audit(version);
      assert.equal(rows.length,1,'exactly one rejection audit must persist after the response');
      assert.ok(rows[0].actor);
      assert.deepEqual(rows[0].detail,{version,sha256:createHash('sha256').update(wasm).digest('hex'),reason});
      assert.equal(await state(),before,'rejection must preserve active/previous/ingress');
      assert.equal(objects.size,objectCount,'rejected artifact must be reclaimed');
      assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${id}'`)).trim(),'1');
      assert.equal((await sql(`SELECT count(*) FROM version_configs WHERE component_id='${id}'`)).trim(),'0');
    }
    const signature = sign(null,Buffer.from(createHash('sha256').update(wasm).digest('hex')),privateKey).toString('base64url');
    const accepted = await send('signed',signature);
    assert.equal(accepted.status,201); await accepted.text();
    assert.deepEqual(await audit('signed'),[]);
    assert.equal((await sql(`SELECT count(*) FROM component_versions WHERE component_id='${id}'`)).trim(),'2');
  } finally {
    assert.equal((await api('/admin/signing-policy',{token,method:'PUT',body:{require_signed_components:false}})).status,200);
    await api('/admin/signing-keys/audit-fixture',{token,method:'DELETE'});
    await api(`/components/${id}`,{token,method:'DELETE'});
  }
  console.log('PASS weak keys rejected without writes; missing, unavailable-key, malformed and mismatched signatures persist audits without publication; valid signatures deploy');
}
