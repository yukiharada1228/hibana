// Test acceptance-time Secret selection with real transactions and Worker reads.
import assert from 'node:assert/strict';
import {createServer, request} from 'node:http';
import {holdComponent, waitForBlocked} from './test-version-lifecycle.mjs';

export async function testSecretSnapshot({api, sql, pg, token, wasm, upload, app, internal, startWorker, stopWorker}) {
  let envGate;
  const proxy = createServer(async (req, res) => {
    // Let rotation commit before the Worker redeems its environment, regardless
    // of which operation acquired the component lock first.
    if (req.url === '/internal/job-env') await envGate;
    const upstream = request(internal+req.url, {method:req.method, headers:req.headers}, response => {
      res.writeHead(response.statusCode, response.headers); response.pipe(res);
    });
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    req.pipe(upstream);
  });
  await new Promise(done => proxy.listen(0, '127.0.0.1', done));
  try {
    await stopWorker();
    await startWorker(`http://127.0.0.1:${proxy.address().port}`, {JOB_ENV_FETCH_TIMEOUT_MS:'10000'});
    for (const mode of ['put','rotate']) {
      for (const first of ['invoke','update']) {
        const name = `snapshot-${mode}-${first}`;
        const created = await api('/components', {method:'POST', token, body:{name}});
        assert.equal(created.status,201);
        const id = (await created.json()).component_id;
        assert.match(id,/^cmp_[a-f0-9]{32}$/);
        const base = `/components/${id}/secrets/RESTORE_TOKEN`;
        assert.equal((await api(base, {method:'PUT', token, body:{value:'restore-test-value'}})).status,201);
        assert.equal((await api(`${base}/deploy-access`, {method:'PUT', token, body:{allowed:true}})).status,200);
        assert.equal((await upload(id,token,'first',wasm,0,{ingress:true,secrets:['RESTORE_TOKEN']})).status,201);
        let releaseEnv;
        envGate = new Promise(done => { releaseEnv = done; });
        const pending = {};
        const send = operation => {
          pending[operation] = operation==='invoke' ? app(name) : api(mode==='put' ? base : `${base}/rotate`, {
            method:mode==='put' ? 'PUT' : 'POST', token, body:{value:'rotated-value'},
          });
          pending[operation].catch(() => {});
        };
        try {
          const release = await holdComponent(pg,id);
          try {
            send(first); await waitForBlocked(sql,1);
            send(first==='invoke' ? 'update' : 'invoke'); await waitForBlocked(sql,2);
          } finally { await release(); }
          assert.equal((await pending.update).status,200);
          releaseEnv();
          const response = await pending.invoke;
          assert.equal(response.status,200);
          assert.equal(response.body.secret,first==='invoke',`${mode}: invocation first retains the old value`);
          assert.equal(response.body.rotated,first==='update',`${mode}: update first supplies the new value`);
          // Check database timestamps directly; fixture never rewrites them.
          const precedes = (await sql(`SELECT v.created_at <= e.created_at FROM executions e
            JOIN function_secrets s ON s.component_id=e.component_id AND s.tenant_id=e.tenant_id
            JOIN function_secret_versions v ON v.secret_id=s.id AND v.tenant_id=s.tenant_id AND v.version=2
            WHERE e.component_id='${id}'`)).trim();
          assert.equal(precedes,first==='update' ? 't' : 'f');
          console.log(`PASS Secret ${mode}, ${first} first: delayed Worker receives the generation valid at HTTP acceptance`);
        } finally {
          releaseEnv(); envGate = undefined;
          await Promise.allSettled(Object.values(pending));
        }
        assert.equal((await api(`/components/${id}`, {method:'DELETE',token})).status,204);
      }
    }
  } finally {
    await stopWorker();
    proxy.closeAllConnections(); await new Promise(done => proxy.close(done));
    await startWorker();
  }
}
