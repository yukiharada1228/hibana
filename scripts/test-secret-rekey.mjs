// Deterministic real-DB races between KEK rewrapping and other Secret mutations.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {holdComponent, waitForBlocked} from './test-version-lifecycle.mjs';

export async function testSecretRekey({api, sql, pg, restart}) {
  const scenarios = [
    {first:'rekey',second:'rotate',reasons:['create','rekey','rotate'],rewrapped:1},
    {first:'rotate',second:'rekey',reasons:['create','rotate'],rewrapped:0},
    {first:'rekey',second:'rekey',reasons:['create','rekey'],rewrapped:1},
    {first:'delete-secret',second:'rekey',reasons:['create'],rewrapped:0},
    {first:'delete-component',second:'rekey',reasons:['create'],rewrapped:0},
  ];
  // One tenant per case keeps the rekey target list independent of other tests.
  for (const [index, scenario] of scenarios.entries()) {
    const slug = `rekey-${index}`;
    const tenantResponse = await api('/admin/tenants', {method:'POST',token:'test-only',body:{
      slug,name:'Rekey regression',admin_email:'test@example.invalid',admin_password:'test-password',
    }});
    assert.equal(tenantResponse.status,201);
    const {tenant_id:tenant,admin_user_id:user} = await tenantResponse.json();
    const login = await api('/auth/login', {method:'POST',body:{tenant_slug:slug,email:'test@example.invalid',password:'test-password'}});
    assert.equal(login.status,201);
    const {token} = await login.json();
    let rekeyToken = token, actor = user;
    if (index === 2) {
      rekeyToken = `disposable-rekey-service-${index}`;
      actor = `tok_rekey_audit_${index}`;
      const hash = createHash('sha256').update(rekeyToken).digest('hex');
      await sql(`INSERT INTO api_tokens(id,tenant_id,user_id,token_hash,scopes,expires_at) VALUES
        ('${actor}','${tenant}',NULL,'${hash}',ARRAY['admin'],now()+interval '10 minutes')`);
    }
    const created = await api('/components', {token,method:'POST',body:{name:'rekey'}});
    assert.equal(created.status,201);
    const id = (await created.json()).component_id;
    assert.equal((await api(`/components/${id}/secrets/TOKEN`, {token,method:'PUT',body:{value:'original-fixture'}})).status,201);
    Object.assign(scenario,{token,rekeyToken,actor,tenant,id});
  }
  const newKid = 'rekey-test';
  await restart({SECRETS_MASTER_KID:newKid,SECRETS_RETIRED_KEYS:`rotated-test:${process.env.SECRETS_MASTER_KEY}`});
  try {
    for (const {token,rekeyToken,actor,tenant,id,first,second,reasons,rewrapped} of scenarios) {
      const base = `/components/${id}`;
      const send = operation => {
        const result = operation==='rekey' ? api('/admin/secrets/rekey',{token:rekeyToken,method:'POST'})
          : operation==='rotate' ? api(`${base}/secrets/TOKEN/rotate`,{token,method:'POST',body:{value:'updated-fixture'}})
          : api(operation==='delete-secret' ? `${base}/secrets/TOKEN` : base,{token,method:'DELETE'});
        result.catch(() => {});
        return result;
      };
      const release = await holdComponent(pg,id);
      const pending = [];
      try {
        pending.push(send(first));
        await waitForBlocked(sql,1);
        pending.push(send(second));
        await waitForBlocked(sql,2);
      } finally {
        await release();
        await Promise.allSettled(pending);
      }
      let actualRewrapped = 0;
      for (const [index,operation] of [first,second].entries()) {
        const response = await pending[index];
        assert.equal(response.status,operation.startsWith('delete-') ? 204 : 200,`${first} then ${second}: ${operation} status`);
        if (operation==='rekey') actualRewrapped += (await response.json()).rewrapped;
      }
      assert.equal(actualRewrapped,rewrapped,'skip deleted or already updated Secrets after acquiring the lock');
      const actors = JSON.parse((await sql(`SELECT coalesce(json_agg(actor ORDER BY id),'[]') FROM audit_logs
        WHERE tenant_id='${tenant}' AND action='secret_rekeyed'`)).trim());
      assert.deepEqual(actors,Array(rewrapped).fill(actor),'rekey audit identifies the user or service token, and skipped work has no success audit');
      const generations = JSON.parse((await sql(`SELECT json_agg(v.reason ORDER BY v.version)
        FROM function_secret_versions v JOIN function_secrets s ON s.tenant_id=v.tenant_id AND s.id=v.secret_id
        WHERE s.component_id='${id}'`)).trim());
      assert.deepEqual(generations,reasons,'never duplicate or overwrite a Secret generation');
      if (!first.startsWith('delete-')) {
        const current = JSON.parse((await sql(`SELECT json_build_object('version',s.current_version,'kid',v.kek_kid)
          FROM function_secrets s JOIN function_secret_versions v ON s.tenant_id=v.tenant_id AND s.id=v.secret_id AND s.current_version=v.version
          WHERE s.component_id='${id}' AND s.deleted_at IS NULL`)).trim());
        assert.deepEqual(current,{version:reasons.length,kid:newKid});
        assert.equal((await api(`${base}/secrets/TOKEN`,{token,method:'DELETE'})).status,204);
      }
      if (first!=='delete-component') assert.equal((await api(base,{token,method:'DELETE'})).status,204);
      console.log(`PASS Secret ${first} then ${second}: serialized generations and rekey count ${rewrapped}`);
    }
  } finally {
    await restart();
  }
}
