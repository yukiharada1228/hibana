// Real KEK rotation while a Worker has yet to redeem its accepted environment.
// All keys and data belong to the disposable HTTP test harness.
import assert from 'node:assert/strict';
import {createServer, request} from 'node:http';

export async function testSecretKeyRetention({api, sql, wasm, upload, url, internal, restart, startWorker, stopWorker}) {
  const oldKid = 'retention-old', newKid = 'retention-new';
  const oldKey = '11'.repeat(32), newKey = '22'.repeat(32);
  const originalKey = `rotated-test:${process.env.SECRETS_MASTER_KEY}`;
  let releaseEnv, reachedEnv, pending;
  const envArrived = new Promise(done => { reachedEnv = done; });
  const envGate = new Promise(done => { releaseEnv = done; });
  const proxy = createServer(async (req,res) => {
    if (req.url === '/internal/job-env') { reachedEnv(); await envGate; }
    const upstream = request(internal+req.url, {method:req.method,headers:req.headers}, response => {
      res.writeHead(response.statusCode,response.headers); response.pipe(res);
    });
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    req.pipe(upstream);
  });
  const oldCount = async () => Number((await sql(`SELECT COALESCE(sum(n),0) FROM secrets_kek_kid_counts_all() WHERE kek_kid='${oldKid}'`)).trim());
  const invoke = () => new Promise((done,reject) => {
    const req = request(url+'/', {headers:{Host:'key-retention.key-retention.hibana.test'}}, res => {
      let body=''; res.on('data', c => body+=c);
      res.on('end', () => { try { done({status:res.statusCode,body:JSON.parse(body)}); } catch (error) { reject(error); } });
      res.on('error',reject);
    });
    req.setTimeout(30000, () => req.destroy(new Error('Key retention fixture deadline')));
    req.on('error',reject); req.end();
  });
  await new Promise(done => proxy.listen(0,'127.0.0.1',done));
  try {
    await restart({SECRETS_MASTER_KID:oldKid,SECRETS_MASTER_KEY:oldKey,SECRETS_RETIRED_KEYS:originalKey});
    assert.equal((await api('/admin/tenants', {method:'POST',token:'test-only',body:{
      slug:'key-retention',name:'Key retention regression',admin_email:'test@example.invalid',admin_password:'test-password',
    }})).status,201);
    const login = await api('/auth/login', {method:'POST',body:{tenant_slug:'key-retention',email:'test@example.invalid',password:'test-password'}});
    assert.equal(login.status,201);
    const {token} = await login.json();
    const created = await api('/components', {token,method:'POST',body:{name:'key-retention'}});
    assert.equal(created.status,201);
    const id = (await created.json()).component_id;
    const secret = `/components/${id}/secrets/RESTORE_TOKEN`;
    assert.equal((await api(secret, {token,method:'PUT',body:{value:'restore-test-value'}})).status,201);
    assert.equal((await api(`${secret}/deploy-access`, {token,method:'PUT',body:{allowed:true}})).status,200);
    assert.equal((await upload(id,token,'first',wasm,0,{ingress:true,secrets:['RESTORE_TOKEN']})).status,201);
    await restart({SECRETS_MASTER_KID:newKid,SECRETS_MASTER_KEY:newKey,SECRETS_RETIRED_KEYS:`${originalKey},${oldKid}:${oldKey}`});
    await stopWorker();
    await startWorker(`http://127.0.0.1:${proxy.address().port}`, {JOB_ENV_FETCH_TIMEOUT_MS:'15000'});
    pending = invoke(); pending.catch(() => {});
    let timer;
    try {
      await Promise.race([envArrived,new Promise((_,reject) => {
        timer = setTimeout(() => reject(new Error('Worker did not request its environment')),5000);
      })]);
    } finally { clearTimeout(timer); }
    assert.equal(await oldCount(),1);
    const rekey = await api('/admin/secrets/rekey', {token,method:'POST'});
    assert.equal(rekey.status,200);
    assert.equal((await rekey.json()).rewrapped,1);
    assert.equal(await oldCount(),1,'rekey must retain the old key while an accepted execution needs it');
    releaseEnv();
    const accepted = await pending;
    assert.equal(accepted.status,200);
    assert.equal(accepted.body.secret,true);
    assert.equal(await oldCount(),0,'completed execution no longer requires the old generation');
    await restart({SECRETS_MASTER_KID:newKid,SECRETS_MASTER_KEY:newKey,SECRETS_RETIRED_KEYS:originalKey});
    const afterRemoval = await invoke();
    assert.equal(afterRemoval.status,200);
    assert.equal(afterRemoval.body.secret,true,'new invocations use the rewrapped generation after old key removal');
    assert.equal((await api(`/components/${id}`, {token,method:'DELETE'})).status,204);
    console.log('PASS KEK rekey retains an accepted invocation\'s old key until completion; removing it afterward preserves real Wasm execution');
  } finally {
    releaseEnv(); await Promise.allSettled([pending]);
    await stopWorker();
    proxy.closeAllConnections(); await new Promise(done => proxy.close(done));
    await restart(); await startWorker();
  }
}
