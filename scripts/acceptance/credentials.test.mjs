import test from 'node:test';
import assert from 'node:assert/strict';
import {issueAcceptanceToken} from './credentials.mjs';
import {issueFixtureToken} from '../test-api-credentials.mjs';

const env = {
  HIBANA_ACCEPTANCE_KUBECONFIG: '/fixture/private kubeconfig',
  HIBANA_ACCEPTANCE_CONTEXT: 'kind-custom-acceptance',
  HIBANA_ACCEPTANCE_SECONDS: '7200',
  HIBANA_ADMIN_EMAIL: 'fixture@example.invalid',
};
function insertedId(query) {
  return query.match(/SELECT '(fixture-[a-f0-9]+)'/)[1];
}

test('acceptance uses the selected context and a token covering load plus recovery', async () => {
  for (const seconds of [30, 7200, 86400]) {
    let query;
    const before = Date.now();
    const issued = await issueAcceptanceToken({...env, HIBANA_ACCEPTANCE_SECONDS: String(seconds)}, async (program, args) => {
      assert.equal(program, 'kubectl');
      assert.deepEqual(args.slice(0, 6), ['--kubeconfig', env.HIBANA_ACCEPTANCE_KUBECONFIG,
        '--context', env.HIBANA_ACCEPTANCE_CONTEXT, '-n', 'hibana']);
      query = args.at(-1);
      return insertedId(query);
    });
    const ttl = seconds + 3600;
    assert.ok(query.includes(`now()+${ttl}*interval '1 second'`));
    assert.ok(Date.parse(issued.expires_at) >= before + ttl * 1000);
    assert.ok(Date.parse(issued.expires_at) <= Date.now() + ttl * 1000);
  }
});

test('acceptance refuses missing context or invalid duration before executing kubectl', async () => {
  for (const changes of [{HIBANA_ACCEPTANCE_CONTEXT: ''}, {HIBANA_ACCEPTANCE_KUBECONFIG: ''},
    {HIBANA_ACCEPTANCE_SECONDS: '0'}, {HIBANA_ACCEPTANCE_SECONDS: '86401'}]) {
    let called = false;
    await assert.rejects(issueAcceptanceToken({...env, ...changes}, async () => { called = true; }));
    assert.equal(called, false);
  }
});

test('ordinary fixture credentials keep the short default and reject unsafe lifetimes', async () => {
  let query;
  await issueFixtureToken(async sql => { query = sql; return insertedId(sql); }, {tenant_slug: 'fixture', email: 'fixture@example.invalid'});
  assert.ok(query.includes("now()+3600*interval '1 second'"));
  for (const ttl_secs of [NaN, Infinity, 59, 90001, 3600.5, '3600']) {
    await assert.rejects(issueFixtureToken(async () => assert.fail('SQL must not run'), {ttl_secs}));
  }
});
