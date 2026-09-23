// Real HTTP/Worker regression with deterministic PostgreSQL lock ordering.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {setTimeout as sleep} from 'node:timers/promises';

export async function holdComponent(pg, id) {
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

export async function waitForBlocked(sql, count) {
  const deadline = Date.now()+4000;
  while (Date.now()<deadline) {
    // Observe PostgreSQL lock waits, independent of ORM SQL formatting.
    // This isolated test has only the component fixture holding a blocking lock.
    const blocked = Number((await sql(`SELECT count(*) FROM pg_stat_activity
      WHERE usename='faas_app' AND wait_event_type='Lock'
      AND cardinality(pg_blocking_pids(pid)) > 0`)).trim());
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
  const listedApp = async () => {
    const response = await api('/components',{token});
    assert.equal(response.status,200);
    return (await response.json()).find(item => item.component_id === id);
  };
  const initial = await listedApp();
  assert.ok(initial, 'an application without a version remains in the list');
  assert.equal(initial.active_version_id,null);
  assert.equal(initial.active_version,null);
  assert.equal(initial.active_version_created_at,null);
  assert.equal((await upload(id,token,'stable',wasm,0,{vars:{GREETING:'stable'},ingress:true})).status,201);
  const snapshot = async () => JSON.parse((await sql(`SELECT json_build_object(
    'active',active_version_id,'previous',previous_active_version_id) FROM components WHERE id='${id}'`)).trim());
  let expectedMessage = 'stable';

  for (const mode of ['active-version','rollback']) {
    for (const first of ['publish','delete']) {
      const version = `${mode}-${first}`;
      const before = await snapshot();
      const uploaded = await upload(id,token,version,wasm,0,{activate:false,vars:{GREETING:version}});
      assert.equal(uploaded.status,201);
      assert.equal(Object.hasOwn(uploaded.data, 'status'), false);
      assert.deepEqual(await snapshot(), before, 'inactive registration must preserve publication');
      const versionId = uploaded.data.version_id;
      assert.match(versionId, /^ver_[a-f0-9]{32}$/);
      const release = await holdComponent(pg,id);
      const pending = {};
      const send = operation => {
        pending[operation] = operation==='delete'
          ? api(`${base}/versions/${mode==='rollback' ? `by-id/${versionId}` : version}`,{token,method:'DELETE'})
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
      const listed = await listedApp();
      const current = await snapshot();
      assert.equal(listed.active_version_id,current.active);
      assert.equal(listed.active_version,expectedMessage);
      assert.ok(Number.isFinite(Date.parse(listed.active_version_created_at)));
      const response = await app('version-race');
      assert.equal(response.status,200,'the surviving public version must keep serving HTTP');
      assert.equal(response.body.message,expectedMessage,'code must retain the surviving version configuration');
      console.log(`PASS ${mode}, ${first} first: publication ${published.status}, deletion ${deleted.status}, live version serves HTTP 200`);
    }
  }
  const live = await snapshot();
  const previous = (await sql(`SELECT version FROM component_versions WHERE id='${live.previous}'`)).trim();
  const listedVersions = async () => {
    const response = await api(`${base}/versions`, {token});
    assert.equal(response.status, 200);
    return response.json();
  };
  const protectedVersions = await listedVersions();
  assert.ok(protectedVersions.every(v => !Object.hasOwn(v, 'status')));
  assert.equal(protectedVersions.find(v => v.version_id===live.active).deletion_blocked_reason,'active_version');
  assert.equal(protectedVersions.find(v => v.version_id===live.previous).deletion_blocked_reason,'rollback_target');
  assert.ok(protectedVersions.filter(v => ![live.active,live.previous].includes(v.version_id))
    .every(v => v.deletion_blocked_reason===null));
  assert.equal((await api(`${base}/versions/${previous}`,{token,method:'DELETE'})).status,409,
    'the previous version remains protected as the rollback destination');
  for (const versionId of [live.active,live.previous])
    assert.equal((await api(`${base}/versions/by-id/${versionId}`,{token,method:'DELETE'})).status,409);

  const idle = await upload(id,token,'inflight-list',wasm,0,{activate:false});
  assert.equal(idle.status,201);
  const idleId = idle.data.version_id;
  await sql(`INSERT INTO executions (id,tenant_id,component_id,version_id,status,http_request)
    SELECT 'version-list-execution',tenant_id,id,'${idleId}','pending',true FROM components WHERE id='${id}'`);
  for (const status of ['pending','running','succeeded']) {
    await sql(`UPDATE executions SET status='${status}' WHERE id='version-list-execution'`);
    const listed = (await listedVersions()).find(v => v.version_id===idleId);
    assert.equal(listed.deletion_blocked_reason,status==='succeeded' ? null : 'active_executions',
      `${status}: the console protection matches the DELETE guard`);
    assert.equal((await api(`${base}/versions/by-id/${idleId}`,{token,method:'DELETE'})).status,
      status==='succeeded' ? 204 : 409);
  }
  assert.ok(!(await listedVersions()).some(v => v.version_id===idleId));
  assert.equal((await sql("SELECT count(*) FROM executions WHERE id='version-list-execution'")).trim(),'1',
    'deleting a version preserves execution history');
  assert.deepEqual(await snapshot(),live,'deleting an unused version preserves publication');
  assert.equal((await app('version-race')).status,200);
  console.log('PASS version list explains active / rollback / pending / running protection; deletion preserves publication and execution history');
  assert.equal((await api(base,{token,method:'DELETE'})).status,204);
}
