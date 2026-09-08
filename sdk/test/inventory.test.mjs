import test from 'node:test';
import assert from 'node:assert/strict';
import { build } from 'esbuild';

// Run the real example in-process for boundary cases. Kubernetes acceptance separately
// exercises its compiled Wasm through the CLI and a real HTTP upstream.
const bundled = await build({entryPoints:[new URL('../examples/inventory-api/src/index.ts',import.meta.url).pathname], bundle:true, platform:'node', format:'esm', write:false});
const {default:app} = await import('data:text/javascript;base64,'+Buffer.from(bundled.outputFiles[0].contents).toString('base64'));
const env = {API_TOKEN:'example-client-token',UPSTREAM_TOKEN:'example-upstream-token',UPSTREAM_URL:'https://inventory.example.com',RELEASE:'v1'};
test('inventory API authenticates before fetching, validates input and exposes only its contract', async () => {
  const original=globalThis.fetch;
  let calls=0, status=200, payload={sku:'PEN-001',name:'Pen',available:42,internal_token:env.UPSTREAM_TOKEN};
  globalThis.fetch=async (url, options) => {
    calls++;
    assert.equal(String(url),'https://inventory.example.com/items/PEN-001');
    assert.equal(options.headers.Authorization,`Bearer ${env.UPSTREAM_TOKEN}`);
    assert.equal(options.redirect,'manual');
    return new Response(JSON.stringify(payload),{status});
  };
  const request=(path,token,vars=env) => app.request(path,{headers:token ? {Authorization:`Bearer ${token}`} : {}},vars);
  try {
    assert.equal((await request('/items/PEN-001')).status,401);
    assert.equal((await request('/items/PEN-001','wrong')).status,401);
    assert.equal((await request('/items/PEN-001',env.API_TOKEN,{...env,UPSTREAM_TOKEN:undefined})).status,503);
    assert.equal((await request('/items/invalid!',env.API_TOKEN)).status,400);
    assert.equal(calls,0);
    const ok=await request('/items/PEN-001?url=http://169.254.169.254',env.API_TOKEN);
    assert.equal(ok.status,200);
    assert.deepEqual(await ok.json(),{sku:'PEN-001',name:'Pen',available:42});
    assert.equal(ok.headers.get('cache-control'),'no-store');
    for (const upstream of [302,401,500]) { status=upstream; assert.equal((await request('/items/PEN-001',env.API_TOKEN)).status,502); }
    status=404; assert.equal((await request('/items/PEN-001',env.API_TOKEN)).status,404);
    status=200;
    for (const bad of [{sku:'WRONG',name:'Pen',available:42},{sku:'PEN-001',name:'Pen',available:-1},{name:'x'.repeat(17000)}]) {
      payload=bad;
      assert.equal((await request('/items/PEN-001',env.API_TOKEN)).status,502);
    }
  } finally { globalThis.fetch=original; }
});
