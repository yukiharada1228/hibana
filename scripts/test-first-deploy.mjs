import assert from 'node:assert/strict';
import {join} from 'node:path';
import {apiClient, deploy} from '../sdk/src/api.mjs';

export async function testFirstDeploy({api, token, url, folder, artifact, app}) {
  // Exercise the actual CLI client against the disposable Control Plane.
  const previous = {HIBANA_CONFIG_HOME:process.env.HIBANA_CONFIG_HOME, HIBANA_PROFILE:process.env.HIBANA_PROFILE};
  let client;
  try {
    process.env.HIBANA_CONFIG_HOME = join(folder,'profiles');
    delete process.env.HIBANA_PROFILE;
    client = await apiClient({url,token});
  } finally {
    for (const [key,value] of Object.entries(previous)) {
      if (value===undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
  let arrived = 0, release;
  const gate = new Promise(done => { release = done; });
  const concurrent = {request:async (...args) => {
    const result = await client.request(...args);
    if (args[0]==='/components' && !args[1] && ++arrived<=2) {
      if (arrived===2) release();
      await gate;
    }
    return result;
  }};
  const name = 'first-deploy-race';
  const config = {name,vars:{},secrets:[],resources:{}};
  const pending = ['first-a','first-b'].map(version => deploy(concurrent,config,artifact,version));
  // Prevent a failed setup request from stranding its peer at the test barrier.
  pending.forEach(result => result.catch(() => release()));
  const settled = await Promise.allSettled(pending);
  const results = settled.map(result => { if (result.status==='rejected') throw result.reason; return result.value; });
  const id = results[0].component_id;
  assert.equal(results[1].component_id,id,'both deploys must share the winning application');
  const components = await client.request('/components');
  assert.equal(components.filter(component => component.name===name).length,1);
  const versions = await client.request(`/components/${id}/versions`);
  assert.deepEqual(versions.map(version => version.version).sort(),['first-a','first-b']);
  assert.equal((await app(name)).status,200,'a concurrent first deploy must leave a serving application');
  const duplicate = await api('/components', {method:'POST',token,body:{name}});
  assert.equal(duplicate.status,409);
  assert.equal((await duplicate.json()).error.code,'conflict');
  assert.equal((await api('/components', {method:'POST',token,body:{name:'invalid name'}})).status,400);
  assert.equal((await api(`/components/${id}`, {method:'DELETE',token})).status,204);
  console.log('PASS concurrent first CLI deploys reuse one application, publish both versions and serve HTTP');
}
