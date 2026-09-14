// Real HTTP/Worker regression with deterministic PostgreSQL lock ordering.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {setTimeout as sleep} from 'node:timers/promises';

async function holdComponent(pg, id) {
  assert.match(pg, /^hibana-http-pg-[0-9]+$/);
  assert.match(id, /^cmp_[a-f0-9]{32}$/);
  const child = spawn('docker', ['exec','-i',pg,'psql','-XqAt','-U','postgres','-d','hibana_http','-v','ON_ERROR_STOP=1'], {
    stdio:['pipe','pipe','pipe'],
  });
  child.stderr.resume();
  child.stdin.on('error', () => {});
  const exited = new Promise(resolve => child.once('close', code => resolve(code)));
  const watchdog = setTimeout(() => child.kill('SIGKILL'), 12000);
  const release = async () => {
    if (!child.stdin.writableEnded) child.stdin.end('COMMIT;\n');
    const code = await exited;
    clearTimeout(watchdog);
    assert.equal(code,0,'test lock holder must exit normally');
  };
  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('Component lock deadline')),5000);
      let output = '';
      child.stdout.on('data', chunk => {
        output += chunk;
        if (output.includes('LOCKED')) { clearTimeout(timer); resolve(); }
      });
      child.once('error', error => { clearTimeout(timer); reject(error); });
      child.once('close', () => { clearTimeout(timer); reject(new Error('Lock holder exited early')); });
      // The server-side deadline also releases the lock if the Docker client dies.
      child.stdin.write(`BEGIN; SET LOCAL idle_in_transaction_session_timeout='10s'; SELECT id FROM components WHERE id='${id}' FOR UPDATE; SELECT 'LOCKED';\n`);
    });
    return release;
  } catch (error) {
    await release();
    throw error;
  }
}

async function waitForBlocked(sql, count) {
  const deadline = Date.now()+4000;
  while (Date.now()<deadline) {
    // Only publication/deletion queries in this isolated CP use this parent lock.
    const blocked = Number((await sql(`SELECT count(*) FROM pg_stat_activity
      WHERE usename='faas_app' AND wait_event_type='Lock'
      AND (query LIKE '%FROM components%FOR UPDATE%' OR query LIKE 'UPDATE components %')`)).trim());
    if (blocked===count) return;
    await sleep(20);
  }
  assert.fail(`Expected ${count} operations waiting for the component lock`);
}

export async function testVersionLifecycle({api, sql, pg, token, wasm, upload, app}) {
  const created = await api('/components',{token,method:'POST',body:{name:'version-race'}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  const base = `/components/${id}`;
  assert.equal((await upload(id,token,'stable',wasm,0,{vars:{GREETING:'stable'},ingress:true})).status,201);
  const snapshot = async () => JSON.parse((await sql(`SELECT json_build_object(
    'active',active_version_id,'previous',previous_active_version_id) FROM components WHERE id='${id}'`)).trim());
  let expectedMessage = 'stable';

  for (const mode of ['active-version','rollback']) {
    for (const first of ['publish','delete']) {
      const version = `${mode}-${first}`;
      const uploaded = await upload(id,token,version,wasm,0,{activate:false,vars:{GREETING:version}});
      assert.equal(uploaded.status,201);
      const versionId = uploaded.data.version_id;
      assert.match(versionId, /^ver_[a-f0-9]{32}$/);
      const before = await snapshot();
      const release = await holdComponent(pg,id);
      const pending = {};
      const send = operation => {
        pending[operation] = operation==='delete'
          ? api(`${base}/versions/${version}`,{token,method:'DELETE'})
          : api(`${base}/${mode}`,{token,method:mode==='rollback' ? 'POST' : 'PUT',body:{version}});
        pending[operation].catch(() => {});
      };
      try {
        send(first);
        await waitForBlocked(sql,1);
        send(first==='publish' ? 'delete' : 'publish');
        await waitForBlocked(sql,2);
      } finally {
        // Always release the transaction and reap requests, including failed assertions.
        await release();
        await Promise.allSettled(Object.values(pending));
      }
      const published = await pending.publish;
      const deleted = await pending.delete;
      const wins = first==='publish';
      assert.equal(published.status,wins ? 200 : mode==='rollback' ? 409 : 404,`${mode}: publication status`);
      assert.equal(deleted.status,wins ? 409 : 204,`${mode}: deletion status`);
      assert.equal((await sql(`SELECT deleted_at IS NOT NULL FROM component_versions WHERE id='${versionId}'`)).trim(),wins ? 'f' : 't');
      assert.deepEqual(await snapshot(),wins ? {active:versionId,previous:before.active} : before,
        'a rejected operation must preserve active and previous pointers');
      assert.equal((await sql(`SELECT count(*) FROM components c JOIN component_versions v
        ON v.id=c.active_version_id WHERE c.id='${id}' AND v.deleted_at IS NOT NULL`)).trim(),'0');
      if (wins) expectedMessage = version;
      const response = await app('version-race');
      assert.equal(response.status,200,'the surviving public version must keep serving HTTP');
      assert.equal(response.body.message,expectedMessage,'code must retain the surviving version configuration');
      console.log(`PASS ${mode}, ${first} first: publication ${published.status}, deletion ${deleted.status}, live version serves HTTP 200`);
    }
  }
  const live = await snapshot();
  const previous = (await sql(`SELECT version FROM component_versions WHERE id='${live.previous}'`)).trim();
  assert.equal((await api(`${base}/versions/${previous}`,{token,method:'DELETE'})).status,409,
    'the previous version remains protected as the rollback destination');
  assert.equal((await api(base,{token,method:'DELETE'})).status,204);
}
