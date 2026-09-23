// Credentials belong only to the disposable acceptance database and its context.
import assert from 'node:assert/strict';
import {runCommand} from '../bounded-process.mjs';
import {issueFixtureToken} from '../test-api-credentials.mjs';

export async function issueAcceptanceToken(env = process.env, execute = runCommand) {
  const kubeconfig = env.HIBANA_ACCEPTANCE_KUBECONFIG;
  const context = env.HIBANA_ACCEPTANCE_CONTEXT;
  const seconds = Number(env.HIBANA_ACCEPTANCE_SECONDS);
  assert.ok(kubeconfig && context, 'The disposable acceptance kubeconfig and context are required');
  assert.ok(Number.isInteger(seconds) && seconds >= 30 && seconds <= 86400);
  return (await issueFixtureToken(query => execute('kubectl', [
    '--kubeconfig', kubeconfig, '--context', context, '-n', 'hibana',
    'exec', 'deployment/hibana-postgres', '--', 'psql', '-XqAt', '-U', 'hibana_admin', '-d', 'hibana',
    '-v', 'ON_ERROR_STOP=1', '-c', query,
  ]), {tenant_slug: 'smoke', email: env.HIBANA_ADMIN_EMAIL, ttl_secs: seconds + 3600})).json();
}
