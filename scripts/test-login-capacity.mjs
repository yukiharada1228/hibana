// Runs against the disposable Control Plane with one async executor thread.
import assert from 'node:assert/strict';
import {createServer, request} from 'node:http';
import {setTimeout as sleep} from 'node:timers/promises';

export async function testIdentityCapacity({api, token}) {
  const session = await (await api('/auth/session', {token})).json();
  assert.ok(session.tenant_id);
  const password = 'identity-capacity-fixture';
  for (const kind of ['user', 'tenant']) {
    const email = i => `capacity-${kind}-${i}@example.invalid`;
    const slug = i => `capacity-tenant-${i}`;
    async function create(i) {
      const response = kind === 'user'
        ? await api(`/tenants/${session.tenant_id}/users`, {method:'POST', token,
          body:{email:email(i), password, role:'member'}})
        : await api('/admin/tenants', {method:'POST', token:'test-only',
          body:{slug:slug(i), name:'Capacity fixture', admin_email:email(i), admin_password:password}});
      const result = await response.json();
      assert.ok([201,429].includes(response.status), JSON.stringify(result));
      if (response.status === 429) {
        assert.equal(response.headers.get('retry-after'),'1');
        assert.equal(result.error.code,'password_capacity');
        assert.equal(result.error.retryable,true);
      }
      return response.status;
    }
    const attempts = Promise.all(Array.from({length:16}, (_, i) => create(i)));
    await sleep(20);
    const started = performance.now();
    assert.equal((await api('/healthz')).status,200);
    const healthMs = Math.round(performance.now()-started);
    const statuses = await attempts;
    assert.ok(statuses.includes(201),'admitted creation must succeed');
    assert.ok(statuses.includes(429),'concurrent creation must have a password work limit');
    assert.equal(await create('recovery'),201,'capacity must recover after hashing completes');
    const login = await api('/auth/login', {method:'POST', body:{
      tenant_slug:kind === 'user' ? session.tenant_slug : slug('recovery'),
      email:email('recovery'), password,
    }});
    assert.equal(login.status,201,'created credentials must be usable');
    assert.ok((await login.json()).token);
    console.log(`PASS ${kind} creation capacity and recovery; health endpoint answered in ${healthMs} ms`);
  }
}

export async function testLoginCapacity({api}) {
  const body = {tenant_slug:'missing-login-fixture',email:'none@example.invalid',password:'wrong-fixture-password'};
  async function login() {
    const response = await api('/auth/login',{method:'POST',body});
    const result = await response.json();
    assert.ok([401,429].includes(response.status), JSON.stringify(result));
    if (response.status === 429) {
      assert.equal(response.headers.get('retry-after'),'1');
      assert.equal(result.error.code,'login_capacity');
      assert.equal(result.error.retryable,true);
    }
    return response.status;
  }
  for (const phase of ['invalid credentials','locked-out IP']) {
    const attempts = Array.from({length:16},login);
    await sleep(20);
    const started = performance.now();
    assert.equal((await api('/healthz')).status,200);
    const healthMs = Math.round(performance.now()-started);
    const statuses = await Promise.all(attempts);
    assert.ok(statuses.includes(429),'concurrent password checks must have a capacity limit');
    assert.ok(statuses.includes(401),'admitted invalid credentials must retain uniform failure');
    console.log(`PASS login capacity during ${phase}; health endpoint answered in ${healthMs} ms`);
    // Exceed the fixture's default five-failure IP lockout before the next burst.
    for (let i=0;i<5;i++) assert.equal(await login(),401);
  }
}

// Model Ingress -> console -> API. The proxy appends both actual hops; an
// outside client's supplied prefix cannot choose the lockout counter.
export async function testProxyLogin({url}) {
  const proxy = createServer((req, res) => {
    const client = req.url === '/alpha' ? '198.51.100.10' : '203.0.113.20';
    const prefix = req.headers['x-forwarded-for'];
    const chain = [prefix, client, '127.0.0.1'].filter(Boolean).join(', ');
    const upstream = request(url + '/auth/login', {method:'POST', headers:{
      'content-type':'application/json', 'x-forwarded-for':chain,
    }}, response => { res.writeHead(response.statusCode, response.headers); response.pipe(res); });
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    req.pipe(upstream);
  });
  await new Promise(done => proxy.listen(0, '127.0.0.1', done));
  const credentials = {tenant_slug:'upload', email:'test@example.invalid', password:'test-password'};
  async function login(client, body, spoof = '') {
    const response = await fetch(`http://127.0.0.1:${proxy.address().port}/${client}`, {
      method:'POST', headers:{'content-type':'application/json', 'x-forwarded-for':spoof},
      body:JSON.stringify(body), signal:AbortSignal.timeout(5000),
    });
    await response.text(); return response.status;
  }
  try {
    assert.equal(await login('beta', credentials), 201);
    for (let i=0; i<10; i++) {
      assert.equal(await login('alpha', {...credentials, email:`absent-${i}@example.invalid`, password:'wrong-fixture'}, '203.0.113.20'), 401);
    }
    assert.equal(await login('alpha', credentials, '192.0.2.99'), 401, 'changing a spoofed prefix must not bypass the IP lockout');
    assert.equal(await login('beta', credentials), 201, 'another client behind the same proxies must still be able to log in');
    console.log('PASS proxy login: independent client lockouts, spoofed prefix cannot bypass or lock out another client');
  } finally {
    proxy.closeAllConnections();
    await new Promise(done => proxy.close(done));
  }
}
