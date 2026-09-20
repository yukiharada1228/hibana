// Runs against the disposable Control Plane in test-http.sh.
import assert from 'node:assert/strict';
import {request} from 'node:http';
import {setTimeout as delay} from 'node:timers/promises';

export async function testJsonReception({url, operator}) {
  const requests = [];
  const timers = [];
  let closed = false;
  try {
    const responses = Array.from({length: 8}, (_, index) => new Promise((resolve, reject) => {
      const trickle = index % 2 === 0;
      const route = index % 2 === 0 ? "/auth/login" : "/admin/tenants";
      const req = request(url + route, {method: 'POST', headers: {
        'Content-Type': index % 2 ? 'application/problem+json' : 'application/json', 'Content-Length': '2097152',
      }, signal: AbortSignal.timeout(15000)}, async res => {
        try {
          const chunks = []; for await (const chunk of res) chunks.push(chunk);
          resolve({status: res.statusCode, body: JSON.parse(Buffer.concat(chunks))});
        } catch (error) { reject(error); }
      });
      requests.push(req);
      req.on('error', reject);
      req.write('{\"password\":\"' + 'a'.repeat(1024 * 1024));
      if (trickle) timers.push(setInterval(() => req.write(' '), 1000));
    }));
    // Observe errors immediately while the maintenance calls below are pending.
    const completed = Promise.all(responses);
    completed.catch(() => {});
    await delay(500);
    const excess = await Promise.all(Array.from({length: 120}, async (_, index) => {
      const response = await fetch(url + (index % 2 ? '/auth/login' : '/admin/tenants'), {
        method: 'POST', headers: {'Content-Type': index % 2 ? 'application/json' : 'application/problem+json'},
        body: '{}', signal: AbortSignal.timeout(3000),
      });
      assert.equal(response.status, 429);
      assert.equal(response.headers.get('retry-after'), '1');
      assert.deepEqual(await response.json(), {error: {
        code: 'json_capacity', message: 'management JSON requests are busy; retry later', retryable: true,
      }});
      return response.status;
    }));
    assert.equal(excess.length, 120);
    const probe = await fetch(url + '/healthz', {headers: {'Content-Type': 'application/json'}, signal: AbortSignal.timeout(3000)});
    assert.equal(probe.status, 200);
    await probe.text();
    const wrongType = await fetch(url + '/auth/login', {method: 'POST', headers: {'Content-Type': 'text/plain'}, body: '{}'});
    assert.equal(wrongType.status, 400);
    await wrongType.text();
    await operator('close', 'json-reception');
    closed = true;
    assert.deepEqual(JSON.parse(await operator('status')), {active_requests: 8, inflight_executions: 0});
    const denied = await fetch(url + '/auth/login', {method: 'POST', body: '{}'});
    assert.equal(denied.status, 503);
    await denied.text();
    for (const response of await completed) {
      assert.equal(response.status, 408);
      assert.deepEqual(response.body, {error: {code: 'request_timeout', message: 'request body reception timed out', retryable: true}});
    }
    assert.deepEqual(JSON.parse(await operator('status')), {active_requests: 0, inflight_executions: 0});
  } finally {
    timers.forEach(clearInterval);
    requests.forEach(req => req.destroy());
    if (closed) await operator('open', 'json-reception');
  }
  await Promise.all(Array.from({length: 8}, async () => {
    const healthy = await fetch(url + '/auth/login', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: '{'});
    assert.equal(healthy.status, 400);
    await healthy.text();
  }));
  console.log('PASS management JSON reception: 8 admitted, 120 rejected, deadlines release capacity and maintenance drain');
}
