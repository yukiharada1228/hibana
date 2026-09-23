// Exercise bounded guest memory with a larger, streamed HTTP response.
import assert from 'node:assert/strict';
import {request} from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';

export async function testStreamAccounting({api, sql, token, wasm, upload, url}) {
  const created = await api('/components', {token, method:'POST', body:{name:'stream-accounting'}});
  assert.equal(created.status,201);
  const id = (await created.json()).component_id;
  const memory = 8 * 1024 * 1024;
  const expectedBytes = 16 * 1024 * 1024;
  const deployed = await upload(id,token,'stream',wasm,0,{
    ingress:true,
    resource_limits:{max_memory_bytes:memory,max_wall_time_ms:10000,max_execution_time_ms:15000},
  });
  assert.equal(deployed.status,201);
  const response = await new Promise((done, reject) => {
    const req = request(url+'/stream-accounting', {headers:{Host:'stream-accounting.upload.hibana.test'}}, res => {
      let bytes = 0;
      res.on('data', chunk => { bytes += chunk.length; });
      res.on('error', reject);
      res.on('end', () => done({status:res.statusCode,bytes}));
    });
    req.setTimeout(20000, () => req.destroy(new Error('Streaming response deadline')));
    req.on('error', reject); req.end();
  });
  assert.deepEqual(response,{status:200,bytes:expectedBytes});
  let execution;
  for (let attempt=0; attempt<100; attempt++) {
    const row = (await sql(`SELECT json_build_object('status',status,'output_bytes',output_bytes,
      'peak_memory_bytes',peak_memory_bytes) FROM executions WHERE component_id='${id}' ORDER BY created_at DESC LIMIT 1`)).trim();
    execution = row ? JSON.parse(row) : undefined;
    if (execution?.status==='succeeded' || execution?.status==='failed') break;
    await sleep(50);
  }
  assert.equal(execution?.status,'succeeded');
  assert.ok(execution.peak_memory_bytes<=memory);
  assert.equal(execution.output_bytes,expectedBytes,'account every streamed byte even when the guest memory limit is smaller');
  assert.equal(Number((await sql(`SELECT sum(output_bytes) FROM usage_rollups WHERE component_id='${id}'`)).trim()),expectedBytes,
    'usage rollups must retain the full streamed byte count too');
  assert.equal((await api(`/components/${id}`,{token,method:'DELETE'})).status,204);
  console.log('PASS 8 MiB guest streams 16 MiB; persisted output accounts for all delivered bytes');
}
