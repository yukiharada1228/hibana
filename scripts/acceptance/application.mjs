import assert from 'node:assert/strict';
import {readFile, writeFile, symlink, appendFile} from 'node:fs/promises';
import {resolve, join} from 'node:path';
import {randomBytes, createHash} from 'node:crypto';
import http from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';
import {apiClient} from '../../sdk/src/api.mjs';
import {runCommand} from '../bounded-process.mjs';

const root=resolve(import.meta.dirname,'../..');
const folder=resolve(['--verify-restored','--verify-lifecycle'].includes(process.argv[2]) ? process.argv[3] : process.env.HIBANA_ACCEPTANCE_FOLDER);
let state;
const savePrivate=(name,value) => writeFile(join(folder,name),JSON.stringify(value),{mode:0o600});
const api=async (path, {method='GET',body,token=state?.adminToken}={}) => {
  const response=await fetch(state.url+path,{method,body:body===undefined ? undefined : JSON.stringify(body),
    headers:{...(token ? {Authorization:`Bearer ${token}`} : {}),...(body===undefined ? {} : {'Content-Type':'application/json'})},signal:AbortSignal.timeout(30000)});
  const data=await response.json().catch(()=>({}));
  if (!response.ok) throw new Error(`API ${method} ${path}: HTTP ${response.status}`);
  return data;
};
const cli=(args,token=state.deployToken,input) => runCommand(process.execPath,[state.cliEntry || process.env.HIBANA_CLI_ENTRY || join(root,'sdk/src/cli.mjs'),...args],{
  cwd:folder,env:{...process.env,HIBANA_URL:state.url,HIBANA_TOKEN:token},input,timeoutMs:180000,
});
const request=(path, token=state.apiToken, extra={}, signal) => new Promise((done,fail)=>{
  const req=http.request(new URL(path,state.gateway),{signal,headers:{Host:'inventory-api.smoke.hibana.local',...(token ? {Authorization:`Bearer ${token}`} : {}),...extra}},res=>{
    let data='';
    res.on('data',b=>{data+=b;if(data.length>32768) req.destroy(new Error('Oversized API response'));});
    res.on('error',fail);
    res.on('end',()=>done({status:res.statusCode,body:data,release:res.headers['x-app-release']}));
  });
  req.setTimeout(15000,()=>req.destroy(new Error('HTTP deadline')));
  req.on('error',fail);req.end();
});
const expected={sku:'PEN-001',name:'Hibana pen',available:42};
async function checkHealthy(release) {
  const r=await request('/items/PEN-001');
  assert.equal(r.status,200,'inventory request failed');
  assert.equal(r.release,release);
  // Compare only a boolean so a regression cannot print Secret plaintext in CI.
  assert.ok(r.body === JSON.stringify(expected),'inventory response must match the public contract');
}
async function approve(version) {
  const origin=new URL(state.upstream);
  await api(`/components/${state.component}/versions/${version}/capabilities/egress`,{
    method:'PUT',body:{allow_outbound:[`${origin.hostname}:${origin.port || (origin.protocol==='https:' ? '443' : '80')}`]},
  });
}
if (process.argv[2] === '--verify-restored') {
  state=JSON.parse(await readFile(join(folder,'client.json'),'utf8'));
  const recovery={passed:false,phase:'warmup',warmup_statuses:{}};
  const began=performance.now();
  try {
    // Maintenance must finish preparation before reopening admission. Even the
    // first health request must succeed; never hide cold-Worker 503s with polling.
    for (let probe=0;probe<10;probe++) {
      const response=await request('/health',null,{},AbortSignal.timeout(10000));
      recovery.warmup_statuses[response.status]=(recovery.warmup_statuses[response.status]||0)+1;
      assert.equal(response.status,200,'admission reopened before applications were ready');
      assert.ok(response.body===JSON.stringify({status:'ok'}),'recovery health contract mismatch');
    }
    recovery.warmup_seconds=(performance.now()-began)/1000;
    recovery.phase='inventory-v2';await checkHealthy('v2');
    recovery.phase='rollback-v1';await cli(['rollback','--version','mvp-v1']);
    recovery.phase='inventory-v1';await checkHealthy('v1');
    recovery.phase='rollback-v2';await cli(['rollback','--version','mvp-v2']);
    recovery.phase='inventory-v2-restored';await checkHealthy('v2');
    recovery.phase='complete';recovery.passed=true;
  } catch (error) {
    if(typeof error.actual==='number') recovery.actual_status=error.actual;
    throw new Error('Post-maintenance acceptance failed; see recovery.json');
  } finally {
    recovery.elapsed_seconds=(performance.now()-began)/1000;
    await writeFile(join(folder,'recovery.json'),JSON.stringify(recovery,null,2)+'\n');
  }
  console.log('PASS live API, Secrets and CLI rollback after verified backup and maintenance');
} else if (process.argv[2] === '--verify-lifecycle') {
  state=JSON.parse(await readFile(join(folder,'client.json'),'utf8'));
  const result={passed:false,startup_statuses:{},phase:'restart'};
  const began=performance.now();
  try {
    // platform start waits for Kubernetes readiness. Active applications are
    // prepared after process startup; record this separately from steady load.
    while (true) {
      const response=await request('/health',null,{},AbortSignal.timeout(10000));
      result.startup_statuses[response.status]=(result.startup_statuses[response.status]||0)+1;
      if (response.status===200) break;
      assert.equal(response.status,503,'unexpected restart response');
      assert.ok(performance.now()-began<120000,'active applications did not become ready');
      await sleep(250);
    }
    result.startup_seconds=(performance.now()-began)/1000;
    await checkHealthy('v2');
    result.phase='delete-scope';
    await assert.rejects(cli(['delete','--all','--yes']), undefined, 'Read+Deploy must not acquire Admin deletion permissions');
    await checkHealthy('v2');
    result.phase='delete-admin';
    await cli(['delete','--all','--yes'],state.adminToken);
    result.phase='verify-deleted';
    const inventory=await api('/components');
    assert.equal((Array.isArray(inventory) ? inventory : inventory.components).length,0,'active applications remain after deletion');
    assert.equal((await request('/health',null)).status,404,'deleted application is still reachable');
    result.passed=true;result.phase='complete';
  } finally {
    await writeFile(join(folder,'lifecycle.json'),JSON.stringify(result,null,2)+'\n');
  }
  console.log('PASS CLI restart preserved applications and Secrets; delete removed applications and HTTP routes');
} else {
  const seconds=Number(process.env.HIBANA_ACCEPTANCE_SECONDS);
  assert.ok(Number.isInteger(seconds) && seconds>=30 && seconds<=86400);
  state={url:process.env.HIBANA_URL,gateway:process.env.GATEWAY,upstream:process.env.HIBANA_UPSTREAM_URL,cliEntry:process.env.HIBANA_CLI_ENTRY,
         apiToken:randomBytes(32).toString('hex'),upstreamToken:process.env.HIBANA_UPSTREAM_TOKEN};
  const tenant=await api('/admin/tenants',{method:'POST',token:process.env.BOOTSTRAP_ADMIN_TOKEN,
    body:{slug:'smoke',name:'MVP acceptance',admin_email:process.env.HIBANA_EMAIL,admin_password:process.env.HIBANA_PASSWORD}});
  state.adminToken=(await api('/auth/login',{method:'POST',token:null,
    body:{tenant_slug:'smoke',email:process.env.HIBANA_EMAIL,password:process.env.HIBANA_PASSWORD}})).token;
  state.tenant=tenant.tenant_id;
  const user=await api(`/tenants/${state.tenant}/users`,{method:'POST',body:{email:'developer@example.invalid',password:randomBytes(32).toString('hex'),role:'member'}});
  state.deployToken=(await api('/tokens',{method:'POST',body:{user_id:user.user_id,scopes:['read','deploy'],ttl_secs:seconds+3600}})).token;
  // All builds and credentials belong to this run; the checked-in example is untouched.
  const source=join(root,'sdk/examples/inventory-api/src/index.ts');
  const vars={UPSTREAM_URL:state.upstream,RELEASE:'v1'};
  const config={name:'inventory-api',main:source,vars,secrets:[],limits:{memory_mb:256,timeout_ms:10000}};
  await savePrivate('hibana.json',config);
  await cli(['build']);
  const artifact=join(folder,'.hibana/build/app.wasm');
  const digest=createHash('sha256').update(await readFile(artifact)).digest('hex');
  delete config.main;config.component=artifact;
  await savePrivate('hibana.json',config);
  await cli(['deploy','--version','mvp-bootstrap']);
  state.component=(await api('/components')).find(c=>c.name==='inventory-api').component_id;
  assert.ok(state.component);
  assert.equal((await request('/health',null)).status,200);
  assert.equal((await request('/items/PEN-001')).status,503,'missing Secrets must fail closed');
  for (const [name,value] of [['API_TOKEN',state.apiToken],['UPSTREAM_TOKEN',state.upstreamToken]]) {
    await cli(['secret','put',name],state.adminToken,value);
    await cli(['secret','allow-deploy',name],state.adminToken);
  }
  config.secrets=['API_TOKEN','UPSTREAM_TOKEN'];
  await savePrivate('hibana.json',config);
  await cli(['deploy','--version','mvp-v1']);
  assert.equal((await request('/items/PEN-001',null)).status,401);
  assert.equal((await request('/items/PEN-001','wrong')).status,401);
  assert.equal((await request('/items/invalid!')).status,400);
  assert.equal((await request('/items/PEN-001')).status,502,'outbound must stay denied before admin approval');
  await approve('mvp-v1');
  await checkHealthy('v1');
  const spoof=await request('/items/PEN-001?url=http://169.254.169.254',state.apiToken,{'x-hibana-env':Buffer.from('{"UPSTREAM_TOKEN":"forged"}').toString('base64url')});
  assert.equal(spoof.status,200);
  assert.ok(spoof.body===JSON.stringify(expected));
  for (const [sku,status] of [['MISSING',404],['REDIRECT',502],['ERROR',502],['LARGE',502],['INVALID',502]]) {
    const r=await request('/items/'+sku);
    assert.equal(r.status,status,sku);
    assert.ok(!r.body.includes(state.upstreamToken) && !r.body.includes(state.apiToken),'Secret must not reach response');
  }
  // Compile a small source change as well as changing vars: rollback must select
  // the old artifact, not just an environment marker on identical bytes.
  await symlink(join(root,'sdk/node_modules'),join(folder,'node_modules'),'dir');
  const nextSource=join(folder,'next.ts');
  await writeFile(nextSource,(await readFile(source,'utf8')).replace('name: item.name, available:', 'name: item.name.trim(), available:'));
  delete config.component;config.main=nextSource;config.vars.RELEASE='v2';
  await savePrivate('hibana.json',config);await cli(['build']);
  const secondDigest=createHash('sha256').update(await readFile(artifact)).digest('hex');
  assert.notEqual(secondDigest,digest,'source update must produce a different artifact');
  delete config.main;config.component=artifact;await savePrivate('hibana.json',config);
  await cli(['deploy','--version','mvp-v2']);
  await approve('mvp-v2');await checkHealthy('v2');
  await cli(['rollback','--version','mvp-v1']);await checkHealthy('v1');
  await cli(['rollback','--version','mvp-v2']);await checkHealthy('v2');
  await savePrivate('client.json',state);
  await runCommand(process.execPath,[join(root,'scripts/acceptance/version-code.mjs'),folder],{timeoutMs:360000});
  console.log('PASS CLI build/deploy, app auth, explicit Secrets/egress, HTTP validation, update and rollback');

  // Fixed concurrency, bounded latency sampling and a graceful in-flight drain at
  // the duration boundary. No external public service receives this load.
  const start=performance.now();
  let count=0,failures=0,latencies=[],worst=0;
  const statuses={};
  const record={wasm_sha256:{v1:digest,v2:secondDigest},requested_seconds:seconds,concurrency:4,pause_ms:100,checks:'auth, Secrets, egress, HTTP errors, vars update and rollback'};
  const loading=Promise.all(Array.from({length:4},async()=>{
    while(failures===0 && performance.now()-start<seconds*1000) {
      const began=performance.now();
      try {
        const r=await request('/items/PEN-001');
        statuses[r.status]=(statuses[r.status]||0)+1;
        if(r.status!==200 || r.body!==JSON.stringify(expected) || r.release!=='v2') failures++;
      } catch { statuses.transport=(statuses.transport||0)+1;failures++; }
      const ms=performance.now()-began;
      count++;worst=Math.max(worst,ms);latencies.push(ms);
      if(latencies.length>20000) latencies.shift();
      await sleep(100);
    }
  }));
  const monitoring=(async()=>{
    while(failures===0 && performance.now()-start<seconds*1000) {
      await sleep(Math.min(30000,Math.max(1,seconds*1000-(performance.now()-start))));
      const ordered=latencies.sort((a,b)=>a-b);latencies=[];
      const sample={seconds:Math.round((performance.now()-start)/1000),count,failures,statuses:{...statuses},p95_ms:ordered[Math.ceil(ordered.length*.95)-1]??null,max_ms:worst};
      await appendFile(join(folder,'load.jsonl'),JSON.stringify(sample)+'\n');
      console.log(JSON.stringify(sample));
    }
  })();
  await loading;await monitoring;
  let finalProbePassed=true;
  try { await checkHealthy('v2'); } catch { finalProbePassed=false; }
  const elapsed=(performance.now()-start)/1000;
  Object.assign(record,{elapsed_seconds:elapsed,count,failures,statuses,max_ms:worst,final_probe_passed:finalProbePassed,
    passed:count>0&&failures===0&&finalProbePassed&&elapsed>=seconds});
  await writeFile(join(folder,'application.json'),JSON.stringify(record,null,2)+'\n');
  assert.ok(record.passed,'sustained HTTP load failed');
}
