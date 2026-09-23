import { issueFixtureToken } from "./test-api-credentials.mjs";
// Uses only the disposable HTTP harness and its fixture credentials.
import assert from 'node:assert/strict';
import {request} from 'node:http';

export async function testPublicNames({api, token, sql}) {
  const counts = () => sql('SELECT (SELECT count(*) FROM tenants), (SELECT count(*) FROM users), (SELECT count(*) FROM components)');
  const before = await counts();
  for (const name of ['', 'TeamA', 'under_score', 'two.labels', '-app', 'app-', '日本語', 'a'.repeat(64)]) {
    const tenant = await api('/admin/tenants', {method:'POST', token:'test-only', body:{
      slug:name, name:'Invalid DNS label fixture', admin_email:'invalid-name@example.invalid', admin_oidc_subject: 'fixture-admin',
    }});
    assert.equal(tenant.status,400, `invalid tenant slug: ${name}`);
    await tenant.json();
    const app = await api('/components', {method:'POST', token, body:{name}});
    assert.equal(app.status,400, `invalid app name: ${name}`);
    await app.json();
  }
  for (const name of [' app', 'app ']) {
    const response = await api('/components', {method:'POST', token, body:{name}});
    assert.equal(response.status,400);
    await response.json();
  }
  assert.equal(await counts(),before,'invalid public names must not leave identity or component rows');
  console.log('PASS tenant/app creation rejects invalid public DNS labels before any database write');
  for (const name of ['0', '2026-api', '9'.repeat(63)]) {
    const response = await api('/components', {method:'POST', token, body:{name}});
    assert.equal(response.status,201, `valid numeric app name: ${name}`);
    const {component_id:id} = await response.json();
    assert.equal((await api(`/components/${id}`, {method:'DELETE',token})).status,204);
  }
  console.log('PASS API accepts the same numeric DNS labels as init, deploy and delete');
}

export async function testHttpMetrics({api}) {
  for (let i=0;i<40;i++) {
    const response = await api('/healthz', {method:`CUSTOM${i}`});
    assert.equal(response.status,405);
    await response.text();
  }
  const response = await api('/metrics');
  assert.equal(response.status,200);
  const metrics = await response.text();
  assert.ok(!metrics.includes('method="CUSTOM'));
  assert.ok(metrics.includes('faas_http_requests_total{method="OTHER",path="/healthz",status="405"} 40'));
  assert.ok(metrics.includes('faas_http_request_duration_seconds_count{method="OTHER",path="/healthz"} 40'));
  console.log('PASS unauthenticated custom HTTP methods share bounded metric labels');
}

export async function testHttpBoundaries({api, sql, token, wasm, upload, url}) {
  const created = await api('/components', {token,method:'POST',body:{name:'request-boundary'}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  assert.equal((await upload(id,token,'1',wasm,0,{ingress:true})).status,201);
  const readerResponse = await issueFixtureToken(sql, {
    tenant_slug:'upload',email:'test@example.invalid',scopes:['read'],
  });
  assert.equal(readerResponse.status,201);
  const reader = await readerResponse.json();
  assert.deepEqual(reader.scopes,['read']);
  const payload = JSON.stringify({password:'fixture-private-password'});
  const result = await new Promise((resolve,reject) => {
    const req = request(url+'/request?code=fixture-private-code&next=%2F', {method:'POST',headers:[
      'Host','request-boundary.upload.hibana.test:12345',
      'Authorization','Bearer fixture-request-token',
      'Cookie','session=fixture-private-cookie','Cookie','csrf=fixture-private-csrf',
      'Accept','text/plain','Accept','application/json',
      'X-Tag','first','X-Tag','caf\u00e9','X-Tag','','X-Tag','\u0080\u00ff','X-Tag','last',
      'Content-Type','application/json','Forwarded','proto=http;host=attacker.invalid',
      'X-Forwarded-Proto','http','X-Forwarded-Host','attacker.invalid',
    ]}, res => {
      let body=''; res.on('data', c => body+=c);
      res.on('end', () => resolve({status:res.statusCode,body:JSON.parse(body)}));
      res.on('error',reject);
    });
    req.on('error',reject);
    req.setTimeout(30000, () => req.destroy(new Error('HTTP boundary fixture deadline')));
    req.end(payload);
  });
  assert.equal(result.status,200);
  assert.equal(result.body.scheme,'Some(Scheme::Https)');
  assert.equal(result.body.authority,'request-boundary.upload.hibana.test');
  assert.equal(result.body.path,'/request?code=fixture-private-code&next=%2F');
  assert.deepEqual(result.body.host,[], 'WASI exposes Host through authority(), not its header fields');
  assert.deepEqual(result.body.authorization,[[...Buffer.from('Bearer fixture-request-token')]]);
  assert.deepEqual(result.body.cookie,['session=fixture-private-cookie','csrf=fixture-private-csrf'].map(v=>[...Buffer.from(v)]));
  assert.deepEqual(result.body.accept,['text/plain','application/json'].map(v=>[...Buffer.from(v)]));
  assert.deepEqual(result.body.tags,['first','caf\u00e9','','\u0080\u00ff','last'].map(v=>[...Buffer.from(v,'latin1')]));
  assert.equal(result.body.body,payload,'the actual application still receives its request body');

  const pageResponse = await api(`/components/${id}/executions`, {token:reader.token});
  assert.equal(pageResponse.status,200);
  const executionId = (await pageResponse.json()).items[0].execution_id;
  assert.match(executionId,/^exec_[a-f0-9]{32}$/);
  assert.equal((await sql(`SELECT input IS NULL AND input_ref IS NULL FROM executions WHERE id='${executionId}'`)).trim(),'t');
  // The detail endpoint must also hide live dispatch data and historical fields,
  // not merely rely on the completion-time cleanup above.
  try {
    for (const status of ['pending','running','succeeded']) {
      await sql(`UPDATE executions SET status='${status}',input='{"headers":{"authorization":"fixture-request-token"},"body":"fixture-private-password"}',
        output='"fixture-private-output"',input_ref='private-input-ref',output_ref='private-output-ref' WHERE id='${executionId}'`);
      const detailResponse = await api(`/executions/${executionId}`,{token:reader.token});
      assert.equal(detailResponse.status,200);
      const detail = await detailResponse.json();
      assert.equal(detail.status,status);
      for (const key of ['input','output','input_ref','output_ref']) assert.ok(!(key in detail),key);
      assert.ok(!JSON.stringify(detail).includes('fixture-private'));
    }
  } finally {
    await sql(`UPDATE executions SET status='succeeded',input=NULL,input_ref=NULL,output_ref=NULL,output=NULL WHERE id='${executionId}'`);
  }
  console.log('PASS real Wasm preserves raw header bytes and repeated values in order, canonical HTTPS origin and private request; Read history excludes payloads in every state; completion discards dispatch input');
}
