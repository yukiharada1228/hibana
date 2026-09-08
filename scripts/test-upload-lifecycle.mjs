// Invoked only by test-http.sh against its disposable PostgreSQL container.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createServer, request} from 'node:http';
import {mkdtemp, readFile, writeFile, rm, open} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {resolve, join} from 'node:path';
import {setTimeout as sleep} from 'node:timers/promises';
import {init} from '../sdk/src/init.mjs';
import {build} from '../sdk/src/build.mjs';
import {loadConfig} from '../sdk/src/config.mjs';
import {runCommand} from './bounded-process.mjs';
import {testVersionEnvironment} from './test-version-environment.mjs';

const pg = process.env.HTTP_TEST_PG_CONTAINER;
assert.match(pg || '', /^hibana-http-pg-[0-9]+$/);
assert.ok(process.env.HTTP_TEST_DATABASE_URL.endsWith('/hibana_http'));
const sql = query => runCommand('docker', ['exec', pg, 'psql', '-XqAt', '-U', 'postgres', '-d', 'hibana_http', '-v', 'ON_ERROR_STOP=1', '-c', query]);
const folder = await mkdtemp(join(tmpdir(), 'hibana-upload-test-'));
const log = await open(join(folder, 'cp.log'), 'w');
const objects = new Map();
let heldPut, onPut, releasePut, cp, worker;
const s3 = createServer(async (req, res) => {
  const key = new URL(req.url, 'http://s3.invalid').pathname;
  if (req.method === 'GET') {
    const bytes = objects.get(key);
    res.writeHead(bytes ? 200 : 404, {'Content-Type': 'application/wasm'}).end(bytes);
    return;
  }
  const bytes = []; for await (const chunk of req) bytes.push(chunk);
  objects.set(key, Buffer.concat(bytes));
  if (heldPut) { onPut(); await heldPut; }
  res.writeHead(200, {'ETag': '"fixture"'}).end();
});
const listen = async server => { await new Promise(done => server.listen(0, '127.0.0.1', done)); return server.address().port; };
const s3Port = await listen(s3);
const vacantPort = async () => { const server = createServer(); const port = await listen(server); await new Promise(done => server.close(done)); return port; };
const port = await vacantPort(), internalPort = await vacantPort(), workerPort = await vacantPort(), metricsPort = await vacantPort();
const url = `http://127.0.0.1:${port}`, internal = `http://127.0.0.1:${internalPort}`;
async function stop(process = cp) {
  if (!process || process.exitCode !== null || process.signalCode !== null) return;
  process.kill('SIGTERM');
  const timer = setTimeout(() => process.kill('SIGKILL'), 3000);
  await new Promise(done => process.once('exit', done)); clearTimeout(timer);
}
async function start() {
  cp = spawn(resolve('target/debug/hibana-control-plane'), [], {
    stdio: ['ignore', log.fd, log.fd], env: {...process.env, BIND_ADDR: `127.0.0.1:${port}`,
      INTERNAL_BIND_ADDR: `127.0.0.1:${internalPort}`, S3_ENDPOINT: `http://127.0.0.1:${s3Port}`,
      RUN_MIGRATIONS: 'false', LOG_FORMAT: 'json', SECRETS_MASTER_KID: 'rotated-test',
      WORKER_HTTP_URL: `http://127.0.0.1:${workerPort}`, INGRESS_BASE_DOMAIN: 'hibana.test'},
  });
  // An absent APP_BIND_ADDR selects the combined management/app listener.
  for (let attempt = 0; attempt < 100; attempt++) {
    if (cp.exitCode !== null) throw new Error('Test Control Plane exited; inspect test log');
    try { if ((await fetch(url + '/healthz', {signal: AbortSignal.timeout(300)})).ok) return; } catch {}
    await sleep(100);
  }
  throw new Error('Test Control Plane startup deadline');
}
async function startWorker() {
  worker = spawn(resolve('target/debug/hibana-worker'), [], {
    stdio: ['ignore', log.fd, log.fd], env: {...process.env,
      WORKER_HTTP_BIND_ADDR: `127.0.0.1:${workerPort}`, METRICS_BIND_ADDR: `127.0.0.1:${metricsPort}`,
      CONTROL_PLANE_INTERNAL_URL: internal, WASM_CACHE_DIR: join(folder, 'cache'), LOG_FORMAT: 'json'},
  });
  for (let attempt = 0; ; attempt++) {
    if (worker.exitCode !== null || attempt === 100) throw new Error('Test Worker failed to start');
    try { await fetch(`http://127.0.0.1:${workerPort}/healthz`, {signal:AbortSignal.timeout(300)}); break; } catch {}
    await sleep(100);
  }
}
const api = async (path, {token, method='GET', body} = {}) => fetch(url + path, {
  method, signal: AbortSignal.timeout(30000), headers: {...(token ? {Authorization: `Bearer ${token}`} : {}), ...(body ? {'Content-Type':'application/json'} : {})},
  body: body ? JSON.stringify(body) : undefined,
});
function holdStorage() {
  heldPut = new Promise(done => { releasePut = () => { heldPut = undefined; done(); }; });
  return new Promise(done => { onPut = done; });
}
function upload(id, token, version, wasm, slowMs=0, fields={}) {
  const boundary = 'hibana-regression';
  const extra = Object.entries(fields).map(([key, value]) => `--${boundary}\r\nContent-Disposition: form-data; name="${key}"\r\n\r\n${JSON.stringify(value)}\r\n`).join('');
  const prefix = Buffer.from(`${extra}--${boundary}\r\nContent-Disposition: form-data; name="version"\r\n\r\n${version}\r\n--${boundary}\r\nContent-Disposition: form-data; name="wasm"; filename="component.wasm"\r\nContent-Type: application/wasm\r\n\r\n`);
  const suffix = Buffer.from(`\r\n--${boundary}--\r\n`);
  const result = new Promise((done, reject) => {
    const req = request(`${url}/components/${id}/versions`, {method:'POST', headers: {Authorization:`Bearer ${token}`,
      'Content-Type':`multipart/form-data; boundary=${boundary}`, 'Content-Length':prefix.length+wasm.length+suffix.length}}, res => {
      let data=''; res.on('data', c => data+=c); res.on('end', () => done({status:res.statusCode, data:JSON.parse(data)}));
    });
    const timeout = setTimeout(() => req.destroy(new Error('Upload deadline')), 35000);
    const send = setTimeout(() => req.end(Buffer.concat([wasm, suffix])), slowMs);
    req.on('close', () => { clearTimeout(timeout); clearTimeout(send); });
    req.on('error', reject); req.write(prefix);
  });
  // Callers inspect the database while this request is pending. Keep a rejection
  // during cleanup from masking the original assertion; awaiting still rejects.
  result.catch(() => {});
  return result;
}
try {
  await init(join(folder, 'app'), {template:'rust'});
  // Test fixture exposes only boolean Secret checks, never the values.
  const source = join(folder, 'app/src/lib.rs');
  await writeFile(source, (await readFile(source, 'utf8')).replace('{ "message": message }', '{ "message": message, "secret": std::env::var("RESTORE_TOKEN").ok().as_deref() == Some("restore-test-value"), "rotated": std::env::var("RESTORE_TOKEN").ok().as_deref() == Some("rotated-value"), "unselected": std::env::var("UNSELECTED").is_ok() }'));
  const artifact = await build(await loadConfig(join(folder, 'app/hibana.json')));
  const wasm = await readFile(artifact);
  // bootstrap::run treats presence of APP_BIND_ADDR as enabled; remove it entirely.
  delete process.env.APP_BIND_ADDR;
  await start();
  await startWorker();
  const created = await api('/admin/tenants', {method:'POST', token:'test-only', body:{slug:'upload',name:'Upload regression',admin_email:'test@example.invalid',admin_password:'test-password'}});
  assert.equal(created.status,201);
  const login = await api('/auth/login', {method:'POST', body:{tenant_slug:'upload',email:'test@example.invalid',password:'test-password'}});
  const {token} = await login.json(); assert.ok(token);
  const component = await (await api('/components', {method:'POST',token,body:{name:'upload'}})).json();
  const id = component.component_id;
  assert.match(id, /^cmp_[a-f0-9]{32}$/);
  assert.equal((await api(`/components/${id}/secrets/RESTORE_TOKEN`, {method:'PUT',token,body:{value:'restore-test-value'}})).status,201);
  const rows = await sql(`SELECT json_build_object('tenant_id',s.tenant_id,'component_id',s.component_id,
    'secret_id',s.id,'name',s.name,'version',v.version,'kek_kid',v.kek_kid,
    'ciphertext',encode(v.ciphertext,'hex'),'nonce',encode(v.nonce,'hex'),
    'wrapped_dek',encode(v.wrapped_dek,'hex'),'dek_nonce',encode(v.dek_nonce,'hex'),'value_len',v.value_len)
    FROM function_secrets s JOIN function_secret_versions v ON v.tenant_id=s.tenant_id AND v.secret_id=s.id AND v.version=s.current_version
    WHERE s.component_id='${id}' AND s.deleted_at IS NULL`);
  const keys = {active_kid:'rotated-test',active_key:process.env.SECRETS_MASTER_KEY,retired:''};
  const verifier = ['--verify-backup-secrets'];
  assert.equal((await runCommand(resolve('target/debug/hibana-control-plane'), verifier, {input:JSON.stringify(keys)+'\n'+rows})).trim(),'1');
  await assert.rejects(runCommand(resolve('target/debug/hibana-control-plane'), verifier,
    {input:JSON.stringify({...keys,active_kid:'k1'})+'\n'+rows}));
  console.log('PASS offline verifier decrypts real persisted secrets with rotated key ID and refuses a mismatched ID');
  const slow = upload(id,token,'1',wasm,16500);
  await sleep(700);
  // Ignore momentary transactions from background reconciliation. An upload
  // holding a connection across its 16.5s body wait is already older than 500ms.
  assert.equal((await sql("SELECT count(*) FROM pg_stat_activity WHERE usename='faas_app' AND state='idle in transaction' AND xact_start < now() - interval '500 milliseconds'")).trim(),'0');
  assert.equal((await slow).status,201, 'upload exceeding the 15s DB idle timeout must succeed');
  const original = [...objects.entries()][0];
  assert.ok(original[0].includes('/versions/ver_'));
  assert.equal((await upload(id,token,'1',Buffer.concat([wasm,Buffer.from([0,2,1,120])]))).status,400);
  assert.deepEqual(objects.get(original[0]),original[1], 'duplicate deploy must not overwrite accepted artifact');
  console.log('PASS slow upload leaves no idle transaction; duplicate version preserves stored artifact');

  assert.equal((await sql("SELECT count(*) FROM executions WHERE component_id='"+id+"'")).trim(),'0',
    'deployment preparation must not invoke the guest');
  assert.equal((await fetch(`http://127.0.0.1:${workerPort}/prepare`, {method:'POST'})).status,401);
  assert.equal((await fetch(`http://127.0.0.1:${workerPort}/prepare`, {method:'HEAD'})).status,401);
  const activeBeforeFailure = (await sql("SELECT active_version_id FROM components WHERE id='"+id+"'")).trim();
  await stop(worker);
  assert.equal((await upload(id,token,'worker-unavailable',wasm)).status,503);
  assert.equal((await sql("SELECT active_version_id FROM components WHERE id='"+id+"'")).trim(),activeBeforeFailure);
  assert.equal((await sql("SELECT count(*) FROM component_versions WHERE component_id='"+id+"' AND version='worker-unavailable'")).trim(),'0');
  await startWorker();
  console.log('PASS preparation requires authentication, never invokes a guest, and preserves publication when a Worker is unavailable');


  const waiting = holdStorage();
  const pending = upload(id,token,'2',wasm);
  await waiting;
  await sql("UPDATE platform_maintenance SET owner='regression' WHERE singleton");
  const maintenance = () => fetch(internal+'/internal/maintenance', {headers:{Authorization:'Bearer test-only'},signal:AbortSignal.timeout(5000)});
  assert.equal((await fetch(internal+'/internal/maintenance')).status,401);
  assert.ok((await (await maintenance()).json()).active_requests > 0);
  assert.equal((await api('/components',{token})).status,503);
  assert.equal((await api('/healthz')).status,200);
  releasePut();
  assert.equal((await pending).status,201);
  assert.equal((await (await maintenance()).json()).active_requests,0);
  await stop(); await start();
  assert.equal((await api('/components',{token})).status,503, 'replacement CP must honor the durable gate');
  const prepareMaintenance = (owner, workers=['127.0.0.1'], bearer='test-only') => fetch(internal+'/internal/maintenance/prepare', {
    method:'POST', headers:{Authorization:`Bearer ${bearer}`,'Content-Type':'application/json'},
    body:JSON.stringify({owner,workers}), signal:AbortSignal.timeout(10000),
  });
  assert.equal((await prepareMaintenance('regression',undefined,'invalid')).status,401);
  assert.equal((await prepareMaintenance('wrong-owner')).status,409);
  assert.equal((await prepareMaintenance('regression',[])).status,400);
  assert.equal((await prepareMaintenance('regression',['127.0.0.2'])).status,503, 'partial/wrong fleet must not be considered prepared');
  const executionsBeforePreparation = await sql('SELECT count(*) FROM executions');
  assert.equal((await prepareMaintenance('regression')).status,204);
  assert.equal(await sql('SELECT count(*) FROM executions'), executionsBeforePreparation, 'maintenance preparation never invokes guest code');
  assert.equal((await api('/components',{token})).status,503, 'preparation itself cannot reopen the gate');
  await sql('UPDATE platform_maintenance SET owner=NULL WHERE singleton');
  assert.equal((await prepareMaintenance('regression')).status,409, 'preparation requires closed admission');
  console.log('PASS shared maintenance gate drains in-flight uploads, preserves internal API and survives CP restart');

  const app = name => new Promise((done, reject) => {
    const req = request(url+'/', {headers:{Host:`${name}.upload.hibana.test`}}, res => {
      let body=''; res.on('data', b => body+=b); res.on('end', () => done({status:res.statusCode, body:JSON.parse(body)}));
    });
    req.setTimeout(30000, () => req.destroy(new Error('HTTP guest deadline')));
    req.on('error', reject); req.end();
  });
  await testVersionEnvironment({api, sql, token, id, wasm, upload, app, holdStorage, releaseStorage:() => releasePut(), url, root:join(folder, 'app'), artifact});

  const waitingPolicy = holdStorage();
  const policyUpload = upload(id,token,'3',wasm); await waitingPolicy;
  await sql("UPDATE tenants SET require_signed_components=true WHERE slug='upload'");
  releasePut(); assert.equal((await policyUpload).status,400);
  assert.equal((await sql("SELECT count(*) FROM component_versions WHERE component_id='"+id+"' AND version='3'")).trim(),'0');
  await sql("UPDATE tenants SET require_signed_components=false WHERE slug='upload'");
  const waitingDelete = holdStorage();
  const deletedUpload = upload(id,token,'4',wasm); await waitingDelete;
  assert.equal((await api('/components/'+id,{method:'DELETE',token})).status,204);
  releasePut(); assert.equal((await deletedUpload).status,404);
  console.log('PASS publication rechecks signature policy and deletion after object storage I/O');
} finally {
  releasePut?.(); await stop(worker); await stop(); s3.closeAllConnections(); await new Promise(done => s3.close(done)); await log.close();
  await rm(folder,{recursive:true,force:true});
}
