// Real HTTP admission must observe publication committed while it waits.
import assert from 'node:assert/strict';
import {holdComponent, waitForBlocked} from './test-version-lifecycle.mjs';

export async function testHttpPublication({api, sql, pg, token, wasm, upload, app}) {
  const name = 'admission-race';
  const created = await api('/components',{token,method:'POST',body:{name}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  assert.match(id,/^cmp_[a-f0-9]{32}$/);
  const base = `/components/${id}`;
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN`,{token,method:'PUT',body:{value:'restore-test-value'}})).status,201);
  assert.equal((await api(`${base}/secrets/RESTORE_TOKEN/deploy-access`,{token,method:'PUT',body:{allowed:true}})).status,200);
  const secrets = ['RESTORE_TOKEN'];
  assert.equal((await upload(id,token,'before',wasm,0,{vars:{GREETING:'before'},secrets,ingress:true})).status,201);
  assert.equal((await upload(id,token,'after',wasm,0,{vars:{GREETING:'after'},secrets,activate:false})).status,201);
  const activate = version => api(`${base}/active-version`,{token,method:'PUT',body:{version}});

  for (const mode of ['active-version','rollback','deploy']) {
    for (const first of ['publish','invoke']) {
      assert.equal((await activate('before')).status,200);
      const version = mode==='deploy' ? `deploy-${first}` : 'after';
      const publish = () => mode==='deploy'
        ? upload(id,token,version,wasm,0,{vars:{GREETING:version},secrets})
        : api(`${base}/${mode}`,{token,method:mode==='rollback' ? 'POST' : 'PUT',body:{version}});
      const release = await holdComponent(pg,id);
      const pending = {};
      const send = operation => {
        pending[operation] = operation==='publish' ? publish() : app(name);
        pending[operation].catch(() => {});
      };
      try {
        send(first);
        await waitForBlocked(sql,1);
        send(first==='publish' ? 'invoke' : 'publish');
        await waitForBlocked(sql,2);
      } finally {
        await release();
        await Promise.allSettled(Object.values(pending));
      }
      assert.equal((await pending.publish).status,mode==='deploy' ? 201 : 200);
      const response = await pending.invoke;
      assert.equal(response.status,200,`${mode}, ${first} first: no transient 404`);
      assert.equal(response.body.message,first==='publish' ? version : 'before');
      assert.equal(response.body.secret,true);
      const after = await app(name);
      assert.equal(after.status,200);
      assert.equal(after.body.message,version);
      console.log(`PASS HTTP admission with ${mode}, ${first} first: selected version stays callable`);
    }
  }

  // Move the fixture generation's timestamp past the waiting transaction's
  // start, as when new configuration becomes visible before it gets the lock.
  // Encryption does not include created_at, so the envelope remains valid.
  const release = await holdComponent(pg,id);
  const pending = app(name);
  pending.catch(() => {});
  try {
    await waitForBlocked(sql,1);
    await sql(`UPDATE function_secret_versions v SET created_at=clock_timestamp()
      FROM function_secrets s WHERE s.id=v.secret_id AND s.tenant_id=v.tenant_id
      AND s.component_id='${id}' AND s.current_version=v.version`);
  } finally {
    await release();
    await Promise.allSettled([pending]);
  }
  const response = await pending;
  assert.equal(response.status,200,'Secret snapshot starts after the component lock wait');
  assert.equal(response.body.secret,true);
  console.log('PASS HTTP acceptance uses the database time after waiting, including newly visible Secrets');

  for (const operation of ['disable','delete']) {
    const release = await holdComponent(pg,id);
    const pending = [];
    try {
      pending.push(operation==='disable'
        ? api(`${base}/ingress`,{token,method:'PUT',body:{enabled:false}})
        : api(base,{token,method:'DELETE'}));
      pending[0].catch(() => {});
      await waitForBlocked(sql,1);
      pending.push(app(name));
      pending[1].catch(() => {});
      await waitForBlocked(sql,2);
    } finally {
      await release();
      await Promise.allSettled(pending);
    }
    assert.equal((await pending[0]).status,operation==='disable' ? 200 : 204);
    assert.equal((await pending[1]).status,404,`${operation} prevents waiting requests from being admitted`);
    if (operation==='disable')
      assert.equal((await api(`${base}/ingress`,{token,method:'PUT',body:{enabled:true}})).status,200);
    console.log(`PASS HTTP admission respects ${operation} committed while waiting`);
  }
}
