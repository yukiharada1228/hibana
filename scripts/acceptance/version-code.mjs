// Make code and environment changes independently observable on the live candidate.
// Run against a private state folder produced by the Kubernetes acceptance harness.
import assert from 'node:assert/strict';
import {readFile, writeFile, mkdtemp} from 'node:fs/promises';
import {resolve, join, basename} from 'node:path';
import {createHash} from 'node:crypto';
import http from 'node:http';
import {runCommand} from '../bounded-process.mjs';

const root=resolve(import.meta.dirname,'../..');
const folder=resolve(process.argv[2]);
const state=JSON.parse(await readFile(join(folder,'client.json'),'utf8'));
const project=await mkdtemp(join(folder,'version-code-'));
const name=basename(project).toLowerCase(); // Do not reuse an earlier proof's versions.
const cli=args=>runCommand(process.execPath,[process.env.HIBANA_CLI_ENTRY || join(root,'sdk/src/cli.mjs'),...args],{
  cwd:project,env:{...process.env,HIBANA_URL:state.url,HIBANA_TOKEN:state.deployToken},timeoutMs:180000,
});
async function check(code, variable) {
  const response=await new Promise((done,fail)=>{
    const request=http.get(state.gateway+'/',{headers:{Host:`${name}.smoke.hibana.local`},signal:AbortSignal.timeout(15000)},incoming=>{
      let body='';
      incoming.on('data',chunk=>{body+=chunk;if(body.length>4096) request.destroy(new Error('Oversized proof response'));});
      incoming.on('error',fail);
      incoming.on('end',()=>done({status:incoming.statusCode,body}));
    });
    request.on('error',fail);
  });
  assert.equal(response.status,200);
  assert.deepEqual(JSON.parse(response.body),{code,variable});
}
const hashes={};
for (const version of ['one','two']) {
  await writeFile(join(project,'index.js'),`export default { fetch(request, env) { return Response.json({ code: ${JSON.stringify('code-'+version)}, variable: env.RELEASE }); } };\n`);
  await writeFile(join(project,'hibana.json'),JSON.stringify({name,main:'index.js',vars:{RELEASE:'vars-'+version},secrets:[],limits:{memory_mb:256,timeout_ms:10000}}));
  await cli(['deploy','--version',version]);
  hashes[version]=createHash('sha256').update(await readFile(join(project,'.hibana/build/app.wasm'))).digest('hex');
  await check('code-'+version,'vars-'+version);
}
assert.notEqual(hashes.one,hashes.two);
await cli(['rollback','--version','one']);
await check('code-one','vars-one');
await cli(['rollback','--version','two']);
await check('code-two','vars-two');
await writeFile(join(folder,'version-code.json'),JSON.stringify({passed:true,component:name,finished_at:new Date().toISOString(),wasm_sha256:hashes,checks:'Read+Deploy CLI update and rollback; different code and vars independently asserted in HTTP body'},null,2)+'\n');
console.log('PASS independently observable code and vars on update and both rollback directions');
