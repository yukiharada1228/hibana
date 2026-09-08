// Mixed-language bounded soak test, called by k8s-local-scale.mjs --soak.
import {readFile, writeFile, appendFile, access} from 'node:fs/promises';
import {resolve} from 'node:path';
import http from 'node:http';
import {setTimeout as delay} from 'node:timers/promises';
import {init} from '../sdk/src/init.mjs';
import {loadConfig} from '../sdk/src/config.mjs';
import {build} from '../sdk/src/build.mjs';
import {apiClient, deploy} from '../sdk/src/api.mjs';

function leb(value) {
  const bytes = [];
  do { let byte = value & 127; value >>>= 7; bytes.push(byte | (value ? 128 : 0)); } while(value);
  return Buffer.from(bytes);
}
function freshComponent(bytes) {
  // A valid Component custom section creates a new cold artifact without changing guest semantics.
  const name = Buffer.from('hibana.soak'), value = Buffer.from(String(Date.now()));
  const content = Buffer.concat([leb(name.length), name, value]);
  return Buffer.concat([bytes, Buffer.from([0]), leb(content.length), content]);
}
const sleep = ms => new Promise(done => setTimeout(done, ms));
function invoke(url, host, echo, slow, signal) {
  return new Promise((done, reject) => {
    const payload = Buffer.alloc(64 * 1024, 97);
    const req = http.request(url + (echo ? '/echo' : '/'), {
      signal, method: echo ? 'POST' : 'GET', headers: {Host: host, ...(echo ? {'Content-Length': payload.length} : {})}
    }, response => {
      let received = Buffer.alloc(0);
      response.on('data', chunk => {
        if (received.length + chunk.length > 1024 * 1024) { req.destroy(new Error('Oversized response')); return; }
        received = Buffer.concat([received, chunk]);
        if (slow) { response.pause(); setTimeout(() => response.resume(), 50); }
      });
      response.on('error', reject);
      response.on('end', () => done({status: response.statusCode, bytes: received, echo: received.equals(payload)}));
    });
    const timer = setTimeout(() => req.destroy(new Error('Request deadline')), 45000);
    req.on('close', () => clearTimeout(timer));
    req.on('error', reject);
    req.end(echo ? payload : undefined);
  });
}
export async function soak({urls, slug, project, command, database, replicas}) {
  const seconds = Number(process.env.HIBANA_SOAK_SECONDS || 7200);
  const concurrency = Number(process.env.HIBANA_SOAK_CONCURRENCY || 4);
  if (!Number.isInteger(seconds) || seconds < 30 || seconds > 86400 || !Number.isInteger(concurrency) || concurrency < 1 || concurrency > 32) {
    throw new Error('Soak duration must be 30..86400 seconds and concurrency 1..32');
  }
  const fixtures = [];
  for (const language of ['hono', 'typescript', 'javascript', 'go', 'rust']) {
    const directory = resolve(project, '..', `soak-${language}`);
    try { await access(resolve(directory, 'hibana.json')); }
    catch { await init(directory, {template: ['typescript', 'javascript'].includes(language) ? 'javascript' : language}); }
    if (language === 'javascript') {
      await writeFile(resolve(directory, 'src/index.js'), 'export default { fetch: async (request) => request.method === "POST" ? new Response(await request.arrayBuffer()) : Response.json({message: "Hello Hibana"}) };\n');
      const path = resolve(directory, 'hibana.json');
      const data = JSON.parse(await readFile(path, 'utf8')); data.main = 'src/index.js';
      await writeFile(path, JSON.stringify(data, null, 2));
    }
    const config = await loadConfig(resolve(directory, 'hibana.json'));
    const api = await apiClient(directory); await api.login();
    const artifact = await build(config);
    await deploy(api, config, artifact, `0.0.0-soak.${Date.now()}`);
    const fixture = {language, api, config, artifact, host: `${config.name}.${slug}.hibana.local`, original: await readFile(artifact)};
    for (const url of urls) {
      const response = await invoke(url, fixture.host, false, false);
      if (response.status !== 200) throw new Error(`Cold ${language} failed: HTTP ${response.status}`);
      fixture.expected = response.bytes;
    }
    fixtures.push(fixture);
  }
  const output = resolve(project, '..', `soak-${Date.now()}.jsonl`);
  await writeFile(output, JSON.stringify({event: 'start', date: new Date().toISOString(), seconds, concurrency, languages: fixtures.map(f => f.language), replicas}) + '\n');
  let stopped = false, interrupted = false, index = 0, failures = 0, count = 0, canceled = 0;
  const controller = new AbortController();
  const options = {signal: controller.signal};
  const boundedCommand = args => command(args, options);
  let window = [];
  const stop = () => { interrupted = true; stopped = true; controller.abort(new Error('Soak interrupted')); };
  process.on('SIGINT', stop); process.on('SIGTERM', stop);
  const began = Date.now(), deadline = began + seconds * 1000;
  const timer = setTimeout(() => controller.abort(new Error('Soak duration reached')), seconds * 1000);
  const loading = Promise.all(Array.from({length: concurrency}, async () => {
    while (!stopped && Date.now() < deadline) {
      const n = index++, fixture = fixtures[n % fixtures.length], start = performance.now();
      const echo = fixture.language !== 'hono' && n % 3 === 0;
      let status = 'transport', valid = false;
      try {
        const response = await invoke(urls[n % urls.length], fixture.host, echo, n % 10 === 0, controller.signal);
        status = response.status;
        valid = status === 200 && (echo ? response.echo : response.bytes.equals(fixture.expected));
      } catch {
        if (controller.signal.aborted) { canceled++; break; }
        // Record ambiguous transport failure; never replay it.
      }
      count++; if (!valid) failures++;
      window.push({ms: performance.now() - start, status, language: fixture.language, valid});
      // Bound the generator's own memory even if metrics collection stalls.
      if (window.length > 20000) window.shift();
      await sleep(20);
    }
  }));
  try {
    let nextDeploy = Date.now() + Math.min(600, seconds / 2) * 1000, revision = 0;
    while (!stopped && Date.now() < deadline) {
      await delay(Math.min(10000, Math.max(0, deadline - Date.now())), undefined, options).catch(() => {});
      if (controller.signal.aborted) break;
      const sample = window; window = [];
      const times = sample.map(s => s.ms).sort((a,b) => a-b);
      const record = {event: 'sample', seconds: Math.round((Date.now() - began) / 1000), count, failures,
        sampled: sample.length, p95_ms: times[Math.ceil(times.length * .95) - 1] ?? null, statuses: {}};
      for (const item of sample) record.statuses[item.status] = (record.statuses[item.status] || 0) + 1;
      try {
        record.database = await database(options);
        const pods = JSON.parse(await boundedCommand(['get', 'pods', '-l', 'app.kubernetes.io/name=hibana-worker', '-o', 'json'])).items;
        record.workers = [];
        for (const pod of pods) {
          const item = {name: pod.metadata.name, restarts: pod.status.containerStatuses?.reduce((sum,c) => sum + c.restartCount, 0) || 0};
          try { item.cache_kib = Number((await boundedCommand(['exec', pod.metadata.name, '--', 'du', '-sk', '/var/cache/hibana'])).split(/\s+/)[0]); } catch { item.cache_kib = null; }
          record.workers.push(item);
        }
        try { record.resources = JSON.parse(await boundedCommand(['get', '--raw', '/apis/metrics.k8s.io/v1beta1/namespaces/hibana/pods'])).items.map(p => ({name: p.metadata.name, containers: p.containers})); } catch { record.resources = null; }
      } catch { record.dependencies_unavailable = true; }
      await appendFile(output, JSON.stringify(record) + '\n');
      console.log(JSON.stringify({seconds: record.seconds, count, failures, p95_ms: record.p95_ms}));
      if (!controller.signal.aborted && Date.now() >= nextDeploy && Date.now() < deadline) {
        const fixture = fixtures[revision++ % fixtures.length];
        const cold = resolve(fixture.config.root, '.hibana/build/soak-cold.wasm');
        await writeFile(cold, freshComponent(fixture.original));
        const api = {...fixture.api, request: (path, args) => fixture.api.request(path, {...args, ...options})};
        await deploy(api, fixture.config, cold, `0.0.0-soak.${Date.now()}`);
        nextDeploy = Date.now() + 600000;
      }
    }
  } catch (error) {
    if (!controller.signal.aborted) throw error;
  } finally {
    stopped = true;
    clearTimeout(timer);
    controller.abort(new Error('Soak finishing'));
    await loading;
    process.off('SIGINT', stop); process.off('SIGTERM', stop);
    await appendFile(output, JSON.stringify({event: 'finish', seconds: Math.round((Date.now() - began)/1000), requested_seconds: seconds, count, failures, canceled, interrupted}) + '\n');
  }
  // Cleanup must have its own budget, independent of the canceled load generator.
  const cleanup = {signal: AbortSignal.timeout(60000)};
  let after;
  do {
    after = await database(cleanup);
    if (after.pending || after.running) await delay(500, undefined, cleanup);
  } while (after.pending || after.running);
  await appendFile(output, JSON.stringify({event: 'drained', database: after}) + '\n');
  console.log(`Soak report: ${output}`);
  if (interrupted) throw new Error('Soak interrupted before requested duration');
  if (!count || failures || after.pending || after.running) throw new Error(`Soak failed: no completed requests, ${failures} failures, or executions remain in flight`);
}
