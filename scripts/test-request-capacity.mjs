// Disposable HTTP harness: incomplete bodies must consume bounded receive
// capacity, expire without creating executions, and release capacity on abort.
import assert from 'node:assert/strict';
import {request} from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';

export async function testRequestCapacity({api, token, sql, wasm, upload, url}) {
  const created = await api('/components', {token, method:'POST', body:{name:'receive-limits'}});
  assert.equal(created.status,201);
  const {component_id:id} = await created.json();
  assert.equal((await upload(id,token,'1',wasm,0,{ingress:true})).status,201);
  const tenant = (await sql(`SELECT tenant_id FROM components WHERE id='${id}'`)).trim();
  const count = async () => Number((await sql(`SELECT count(*) FROM executions WHERE component_id='${id}'`)).trim());
  const quotas = async body => {
    const response = await api(`/admin/tenants/${tenant}/quotas`, {method:'PUT',token:'test-only',body});
    assert.equal(response.status,200); await response.text();
  };
  const status = async value => {
    const response = await api(`/admin/tenants/${tenant}/status`, {method:'PUT',token:'test-only',body:{status:value}});
    assert.equal(response.status,200); await response.text();
  };
  const requests=[];
  function begin(partial = false) {
    let req;
    const result = new Promise((resolve,reject) => {
      req=request(url+(partial?'/echo':'/'), {method:partial?'POST':'GET',headers:{
        Host:'receive-limits.upload.hibana.test', ...(partial?{'Content-Length':'8388608'}:{}),
      }},res=>{
        let body='';res.on('data',chunk=>body+=chunk);
        res.on('end',()=>resolve({status:res.statusCode,body}));res.on('error',reject);
      });
      req.on('error',reject);
      req.setTimeout(15000,()=>req.destroy(new Error('receive fixture deadline')));
      if(partial)req.write('x');else req.end();
    });
    result.catch(()=>{}); requests.push(req);
    return {req,result};
  }
  async function normal() {
    for(let i=0;i<50;i++) {
      const response=await begin().result;
      if(response.status===200)return;
      assert.ok([429,503].includes(response.status),JSON.stringify(response));
      await sleep(50);
    }
    assert.fail('request capacity did not recover');
  }
  try {
    await quotas({max_concurrent_executions:1});
    const held=begin(true);
    await sleep(200);
    const rejected=await begin(true).result;
    assert.equal(rejected.status,429);
    assert.equal(JSON.parse(rejected.body).error.code,'concurrency_limit');
    assert.equal(await count(),0);
    held.req.destroy(); await held.result.catch(()=>{});
    await normal();
    console.log('PASS incomplete bodies count toward per-tenant reception capacity and disconnect releases it');

    const before=await count();
    const suspendedWhileReceiving=begin(true); await sleep(200);
    await status('suspended');
    assert.equal((await begin(true).result).status,404,'suspended tenant must remain hidden before receiving a body');
    suspendedWhileReceiving.req.end(Buffer.alloc(8388607,120));
    assert.equal((await suspendedWhileReceiving.result).status,401,'suspension during receipt must prevent execution');
    assert.equal(await count(),before);
    await status('active');

    await quotas({max_concurrent_executions:20});
    const heldMany=Array.from({length:8},()=>begin(true));
    await sleep(250);
    const overloaded=await begin(true).result;
    assert.equal(overloaded.status,503);
    assert.equal(JSON.parse(overloaded.body).error.code,'request_capacity');
    assert.equal(await count(),before);
    const timedOut=await Promise.all(heldMany.map(p=>p.result));
    assert.ok(timedOut.every(r=>r.status===408),JSON.stringify(timedOut));
    assert.equal(await count(),before);
    await normal();
    console.log('PASS CP receive capacity rejects excess bodies, all slow bodies expire with 408, and no executions are created');

    await quotas({invoke_rate_per_sec:0,invoke_burst:0});
    const limited=await begin(true).result;
    assert.equal(limited.status,429);
    assert.equal(JSON.parse(limited.body).error.code,'rate_limited');
    console.log('PASS invocation rate limits reject before reading an incomplete body');
  } finally {
    for(const req of requests)req.destroy();
    await status('active'); await quotas({});
  }
}
