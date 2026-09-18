// Run only against test-http.sh's disposable services.
import assert from 'node:assert/strict';
import {request} from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';

export async function testUploadCapacity({api, token, wasm, upload, url, holdStorage, releaseStorage}) {
  const create = async token => {
    const response = await api('/components', {token, method:'POST', body:{name:'upload-capacity'}});
    assert.equal(response.status,201);
    return (await response.json()).component_id;
  };
  const id = await create(token);
  const tenant = await api('/admin/tenants', {token:'test-only',method:'POST',body:{
    slug:'upload-capacity',name:'Upload capacity fixture',admin_email:'upload@example.invalid',admin_password:'fixture-password',
  }});
  assert.equal(tenant.status,201); await tenant.text();
  const login = await api('/auth/login', {method:'POST',body:{tenant_slug:'upload-capacity',email:'upload@example.invalid',password:'fixture-password'}});
  assert.equal(login.status,201);
  const {token:otherToken} = await login.json();
  const otherId = await create(otherToken);
  const requests = [];
  let storageHeld = false;
  function begin(id, token, complete = false) {
    const boundary = 'upload-capacity-fixture';
    const prefix = `--${boundary}\r\nContent-Disposition: form-data; name="version"\r\n\r\ninvalid-fixture\r\n--${boundary}\r\nContent-Disposition: form-data; name="wasm"; filename="app.wasm"\r\n\r\nx`;
    const suffix = `\r\n--${boundary}--\r\n`;
    let req;
    const result = new Promise((done,reject) => {
      req = request(`${url}/components/${id}/versions`, {method:'POST',headers:{
        Authorization:`Bearer ${token}`, 'Content-Type':`multipart/form-data; boundary=${boundary}`,
        'Content-Length':Buffer.byteLength(prefix)+(complete?Buffer.byteLength(suffix):1024),
      }},res=>{
        let body=''; res.on('data',c=>body+=c);
        res.on('end',()=>done({status:res.statusCode,body,retryAfter:res.headers['retry-after']}));
        res.on('error',reject);
      });
      req.on('error',reject);
      req.setTimeout(10000,()=>req.destroy(new Error('upload capacity fixture deadline')));
      if(complete)req.end(prefix+suffix);else req.write(prefix);
    });
    result.catch(()=>{}); requests.push(req);
    return {req,result};
  }
  const rejected = async (id, token, status) => {
    const response = await begin(id,token).result;
    assert.equal(response.status,status,JSON.stringify(response));
    assert.equal(JSON.parse(response.body).error.code,'upload_capacity');
    assert.equal(response.retryAfter,'1');
  };
  async function recovered(id, token) {
    for(let i=0;i<50;i++) {
      const response = await begin(id,token,true).result;
      if(response.status===400)return; // Invalid Wasm reaches validation once a slot is free.
      assert.ok([429,503].includes(response.status),JSON.stringify(response));
      await sleep(50);
    }
    assert.fail('upload capacity did not recover');
  }
  try {
    const held = [begin(id,token),begin(id,token)];
    await sleep(200);
    await rejected(id,token,429);
    held.push(begin(otherId,otherToken),begin(otherId,otherToken));
    await sleep(200);
    await rejected(id,token,503);
    held[0].req.destroy(); await held[0].result.catch(()=>{});
    await recovered(id,token);
    for(const pending of held)pending.req.destroy();
    await Promise.all(held.map(p=>p.result.catch(()=>{})));
    await recovered(id,token); await recovered(otherId,otherToken);
    assert.equal((await upload(id,token,'1',wasm)).status,201);
    console.log('PASS upload capacity is bounded per tenant and CP; disconnects and invalid Wasm release capacity');

    const stored = holdStorage(); storageHeld = true;
    const publishing = upload(id,token,'2',wasm);
    await stored;
    const partial = begin(id,token); await sleep(200);
    await rejected(id,token,429);
    partial.req.destroy(); await partial.result.catch(()=>{});
    releaseStorage(); storageHeld = false;
    assert.equal((await publishing).status,201);
    await recovered(id,token);
    assert.equal((await upload(id,token,'3',wasm)).status,201);
    console.log('PASS upload reservations survive reception until storage and publication finish, then release');
  } finally {
    for(const req of requests)req.destroy();
    if(storageHeld)releaseStorage();
  }
}
