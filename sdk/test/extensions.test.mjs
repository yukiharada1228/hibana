import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, readFile, readdir, mkdir, rm, symlink, access, realpath } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { build, build as bundle } from 'esbuild';
import { build as buildApplication } from '../src/build.mjs';
import { loadConfig } from '../src/config.mjs';
import { resolveExtensions, prepareExtensionWit } from '../src/extensions.mjs';
import { HTTP_CONTRACT } from '../src/extension-manifest.mjs';
import { dev, shouldRebuild } from '../src/dev.mjs';

test('extension configuration rejects malformed interfaces and platform compatibility flags', async () => {
  const root = await mkdtemp(join(tmpdir(), 'hibana-extensions-'));
  const path = join(root, 'hibana.json');
  try {
    for (const extensions of [null, { packages: ['example'] }, ['a', 'a'], [null], ['../bad'], ['https://example.com'], ['a@1.0.0']]) {
      await writeFile(path, JSON.stringify({ name: 'app', main: 'index.mjs', extensions }));
      await assert.rejects(loadConfig(path), undefined, JSON.stringify(extensions));
    }
    await writeFile(path, JSON.stringify({ name: 'app', main: 'index.mjs', compatibility_flags: ['nodejs_compat'] }));
    await assert.rejects(loadConfig(path), /Unsupported/);
    await writeFile(path, JSON.stringify({ name: 'app', component: 'a.wasm', extensions: ['./extension'] }));
    const config = await loadConfig(path);
    config.build = { watch: ['src'] };
    assert.equal(shouldRebuild(config, 'extension/dist/plug.wasm'), true);
    assert.equal(shouldRebuild(config, 'extension/hibana.extension.json'), true);
    assert.equal(shouldRebuild(config, 'extension/target/partial.wasm'), false);
    assert.equal(shouldRebuild(config, 'outside/plug.wasm'), false);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('Node imports require application aliases, which resolve in the application project', async () => {
  const root = await mkdtemp(join(tmpdir(), 'hibana-extension-resolution-'));
  try {
    await mkdir(join(root, 'compat'));
    await writeFile(join(root, 'compat/value.mjs'), 'export const value = "from application";');
    const options = { stdin: { contents: 'export { value } from "node:user-module";', resolveDir: root }, bundle: true, platform: 'browser', format: 'esm', write: false, logLevel: 'silent' };
    await assert.rejects(build(options), /Could not resolve/);
    await writeFile(join(root, 'compat/hibana.extension.json'), JSON.stringify({
      schemaVersion: 1, runtime: HTTP_CONTRACT, aliases: { 'node:user-module': './value.mjs' },
    }));
    const plan = await resolveExtensions({ root, main: 'index.mjs', extensions: ['./compat'] });
    const result = await build({ ...options, absWorkingDir: root, alias: plan.aliases, external: plan.imports });
    const module = await import('data:text/javascript;base64,' + Buffer.from(result.outputFiles[0].text).toString('base64'));
    assert.equal(module.value, 'from application');
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('failed JavaScript compilation removes staging and preserves the published artifact', async () => {
  const root = await mkdtemp(join(tmpdir(), 'hibana-js-build-failure-'));
  try {
    const output = join(root, '.hibana/build');
    await mkdir(output, { recursive: true });
    const previous = Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]);
    await writeFile(join(output, 'app.wasm'), previous);
    await writeFile(join(root, 'index.mjs'), 'export default {');
    await assert.rejects(buildApplication({ root, main: 'index.mjs' }), /Build failed/);
    assert.deepEqual(await readFile(join(output, 'app.wasm')), previous);
    assert.deepEqual(await readdir(output), ['app.wasm']);
  } finally { await rm(root, { recursive: true, force: true }); }
});

async function fixture(t) {
  const root = await realpath(await mkdtemp(join(tmpdir(), 'hibana-extension-package-')));
  t.after(() => rm(root, { recursive: true, force: true }));
  const config = { root, main: 'index.mjs', extensions: ['@example/compat'] };
  async function install(extra = {}, name = '@example/compat', packageFields = {}, parent = root) {
    const folder = join(parent, 'node_modules', name);
    await mkdir(folder, { recursive: true });
    await writeFile(join(folder, 'package.json'), JSON.stringify({ name, version: '1.0.0', exports: { '.': './throw.mjs', './hibana.extension.json': './hibana.extension.json' }, ...packageFields }));
    await writeFile(join(folder, 'throw.mjs'), 'throw new Error("Extension host entry point must never execute");');
    await writeFile(join(folder, 'value.mjs'), 'export const value = "from packaged extension";');
    await writeFile(join(folder, 'hibana.extension.json'), JSON.stringify({ schemaVersion: 1, runtime: HTTP_CONTRACT, aliases: { 'node:example': './value.mjs' }, ...extra }));
    return folder;
  }
  return { root, config, install };
}

test('declared extension dependencies initialize first, deduplicate and propagate requirements without running package code', async t => {
  const f = await fixture(t);
  const transport = await f.install({ aliases: { net: './value.mjs' }, preload: ['./value.mjs'], permissions: ['outbound-network'] }, '@example/transport');
  const database = await f.install({ schemaVersion: 2, dependencies: ['@example/transport'], aliases: { pg: './value.mjs' }, preload: ['./value.mjs'] }, '@example/compat', { dependencies: { '@example/transport': '1.0.0', 'ordinary-js-dependency': '1.0.0' } });
  const config = { ...f.config, extensions: ['@example/compat', '@example/transport'] };
  const before = JSON.stringify(config);
  const plan = await resolveExtensions(config);
  assert.deepEqual(plan.preload, [join(transport, 'value.mjs'), join(database, 'value.mjs')]);
  assert.deepEqual(Object.keys(plan.aliases), ['net', 'pg']);
  assert.deepEqual(plan.permissions, ['outbound-network']);
  assert.deepEqual(plan.metadata, {schema_version:1,input:'javascript',roots:['@example/compat','@example/transport'],extensions:[
    {name:'@example/transport',version:'1.0.0',dependencies:[],permissions:['outbound-network']},
    {name:'@example/compat',version:'1.0.0',dependencies:['@example/transport'],permissions:[]},
  ]});
  assert.ok(!JSON.stringify(plan.metadata).includes(f.root));
  assert.equal(JSON.stringify(config), before);
  assert.equal(plan.net_allow_outbound, undefined);
  await assert.rejects(resolveExtensions(f.config, { mode: 'dev' }), /dev does not currently allow/);
});

test('diamond dependencies are included once in deterministic dependency order', async t => {
  const f = await fixture(t);
  const common = await f.install({ aliases: {}, preload: ['./value.mjs'] }, '@example/common');
  const parents = [];
  for (const name of ['@example/compat', '@example/other']) {
    parents.push(await f.install({ schemaVersion: 2, dependencies: ['@example/common'], aliases: {}, preload: ['./value.mjs'] }, name, { dependencies: { '@example/common': '1.0.0' } }));
  }
  const plan = await resolveExtensions({ ...f.config, extensions: ['@example/compat', '@example/other'] });
  assert.deepEqual(plan.preload, [common, ...parents].map(folder => join(folder, 'value.mjs')));
});

test('local metadata keeps relative names, missing versions and canonical references to shared packages', async t => {
  const f = await fixture(t);
  const shared = await f.install({ aliases: {} }, '@example/shared', { version: '2.3.4-rc.1' });
  await symlink(shared, join(f.root, 'linked'));
  await mkdir(join(f.root, 'local'));
  await writeFile(join(f.root, 'local/hibana.extension.json'), JSON.stringify({ schemaVersion: 1, runtime: HTTP_CONTRACT }));
  await f.install({ schemaVersion: 2, aliases: {}, dependencies: ['@example/shared'] }, '@example/compat', { dependencies: { '@example/shared': '^2.0.0' } });
  const plan = await resolveExtensions({ ...f.config, extensions: ['./linked', '@example/compat', '@example/shared', './local'] });
  assert.deepEqual(plan.metadata, { schema_version: 1, input: 'javascript', roots: ['./linked', '@example/compat', './local'], extensions: [
    { name: './linked', version: '2.3.4-rc.1', dependencies: [], permissions: [] },
    { name: '@example/compat', version: '1.0.0', dependencies: ['./linked'], permissions: [] },
    { name: './local', version: null, dependencies: [], permissions: [] },
  ] });
});

test('dependencies resolve from their owning package; incompatible installations are rejected', async t => {
  const f = await fixture(t);
  const parent = await f.install({ schemaVersion: 2, dependencies: ['@example/transport'], aliases: {} }, '@example/compat', { dependencies: { '@example/transport': '1.0.0' } });
  const nested = await f.install({ aliases: { net: './value.mjs' } }, '@example/transport', {}, parent);
  await f.install({ aliases: { net: './value.mjs' } }, '@example/transport', { version: '2.0.0' });
  assert.equal((await resolveExtensions(f.config)).aliases.net, join(nested, 'value.mjs'));
  await assert.rejects(resolveExtensions({ ...f.config, extensions: ['@example/compat', '@example/transport'] }), /Multiple installations/);
});

test('extension dependency declarations reject cycles, undeclared packages and non-package references', async t => {
  const f = await fixture(t);
  const graph = { schemaVersion: 2, aliases: {}, dependencies: ['@example/other'] };
  await f.install(graph);
  await assert.rejects(resolveExtensions(f.config), /Declare @example\/other in package.json/);
  await f.install(graph, '@example/compat', { dependencies: { '@example/other': '1.0.0' } });
  await assert.rejects(resolveExtensions(f.config), /Cannot resolve/);
  await f.install({ ...graph, dependencies: ['@example/compat'] }, '@example/other', { dependencies: { '@example/compat': '1.0.0' } });
  await assert.rejects(resolveExtensions(f.config), /dependency cycle: @example\/compat -> @example\/other -> @example\/compat/);
  for (const dependencies of [['./local'], ['https://example.com'], ['x@1.0.0'], ['x', 'x'], [null], 'x']) {
    await f.install({ ...graph, dependencies });
    await assert.rejects(resolveExtensions(f.config), /dependencies must list/);
  }
  await f.install({ dependencies: [] });
  await assert.rejects(resolveExtensions(f.config), /require schemaVersion 2/);
});

test('the extension limit includes indirect dependencies', async t => {
  const f = await fixture(t);
  for (let i = 0; i < 65; i++) {
    const name = i === 0 ? '@example/compat' : `@example/chain-${i}`;
    const next = `@example/chain-${i + 1}`;
    await f.install({ schemaVersion: 2, aliases: {}, dependencies: i < 64 ? [next] : [] }, name, { dependencies: { [next]: '1.0.0' } });
  }
  await assert.rejects(resolveExtensions(f.config), /At most 64 extensions/);
});

test('installed packages provide aliases without executing host entry points or mutating config', async t => {
  const f = await fixture(t);
  await f.install({ preload: ['./value.mjs'] });
  const before = JSON.stringify(f.config);
  const config = await resolveExtensions(f.config);
  const result = await bundle({ stdin: { contents: 'export { value } from "node:example";', resolveDir: f.root }, bundle: true, platform: 'browser', format: 'esm', write: false, absWorkingDir: f.root, alias: config.aliases, external: config.imports });
  const module = await import('data:text/javascript;base64,' + Buffer.from(result.outputFiles[0].text).toString('base64'));
  assert.equal(module.value, 'from packaged extension');
  assert.equal(JSON.stringify(f.config), before);
  assert.deepEqual(config.permissions, []);
  await assert.rejects(access(join(f.root, '.hibana')), { code: 'ENOENT' });
});

test('unsupported contracts, permissions and incomplete package inputs fail before replacing an artifact', async t => {
  const f = await fixture(t);
  const destination = join(f.root, '.hibana/build');
  await mkdir(destination, { recursive: true });
  await writeFile(join(destination, 'app.wasm'), 'previous artifact');
  for (const [extra, pattern] of [
    [{ schemaVersion: 3 }, /schemaVersion/],
    [{ runtime: 'wasi:http/incoming-handler@0.3.0' }, /runtime contract/],
    [{ permissions: ['filesystem'] }, /Unsupported permissions/],
    [{ permissions: ['outbound-network', 'outbound-network'] }, /Unsupported permissions/],
    [{ install: 'node hook.mjs' }, /Unsupported manifest field/],
    [{ aliases: { 'node:example': '../value.mjs' } }, /inside the package/],
    [{ aliases: { 'node:example': './missing.mjs' } }, /ENOENT/],
    [{ imports: ['example:hash/api@1.0.0'], wit: './wit' }, /prebuilt components/],
  ]) {
    await f.install(extra);
    await assert.rejects(buildApplication(f.config), pattern);
    assert.equal(await readFile(join(destination, 'app.wasm'), 'utf8'), 'previous artifact');
  }
  const folder = await f.install();
  await writeFile(join(folder, 'hibana.extension.json'), '{"secret":"do-not-print", broken');
  await assert.rejects(buildApplication(f.config), error => /valid JSON/.test(error.message) && !error.message.includes('do-not-print'));
});

test('package paths cannot escape through symlinks; alias and interface conflicts are explicit', async t => {
  const f = await fixture(t);
  const folder = await f.install();
  await writeFile(join(f.root, 'outside.mjs'), 'outside');
  await symlink(join(f.root, 'outside.mjs'), join(folder, 'link.mjs'));
  await f.install({ aliases: { 'node:example': './link.mjs' } });
  await assert.rejects(resolveExtensions(f.config), /including symlinks/);
  await f.install();
  await f.install({}, '@example/other');
  await assert.rejects(resolveExtensions({ ...f.config, extensions: ['@example/compat', '@example/other'] }), /alias conflict/);
});

test('permission declarations never grant access, and dev rejects network requirements before downloading a runtime', async t => {
  const f = await fixture(t);
  await f.install({ permissions: ['outbound-network'] });
  const config = await resolveExtensions(f.config);
  assert.deepEqual(config.permissions, ['outbound-network']);
  assert.equal(config.net_allow_outbound, undefined);
  let built = false;
  await assert.rejects(dev(f.config, {}, async () => { built = true; }), /dev does not currently allow/);
  assert.equal(built, false);
  await assert.rejects(access(join(f.root, '.hibana')), { code: 'ENOENT' });
});

test('package WIT is assembled in staging; project and package sources are unchanged', async t => {
  const f = await fixture(t);
  const folder = await f.install({ imports: ['example:hash/api@1.0.0'], wit: './wit', components: ['./hash.wasm'] });
  await mkdir(join(folder, 'wit'));
  const definition = 'package example:hash@1.0.0; interface api { hash: func(input: string) -> string; }';
  await writeFile(join(folder, 'wit/hash.wit'), definition);
  await writeFile(join(folder, 'hash.wasm'), Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]));
  const resolved = await resolveExtensions(f.config);
  const wit = await prepareExtensionWit(resolved, join(f.root, 'staging'));
  const world = await readFile(join(wit, 'world.wit'), 'utf8');
  assert.match(world, /import example:hash\/api@1.0.0;/);
  assert.ok(world.includes(`export ${HTTP_CONTRACT};`));
  assert.equal(await readFile(join(folder, 'wit/hash.wit'), 'utf8'), definition);
  await access(join(wit, 'deps/http/types.wit'));
  await assert.rejects(resolveExtensions({ ...f.config, main: undefined, component: 'app.wasm' }), /main entry/);
  await writeFile(join(f.root, 'outside.wit'), 'not a package input');
  await symlink(join(f.root, 'outside.wit'), join(folder, 'wit/linked.wit'));
  await assert.rejects(resolveExtensions(f.config), /without symlinks/);
});

test('package names are explicit and lockfile changes rebuild native projects too', async t => {
  const f = await fixture(t);
  const path = join(f.root, 'hibana.json');
  for (const packages of [['../outside'], ['https://example.com/plugin'], ['a@1.0.0'], ['a', 'a'], [null]]) {
    await writeFile(path, JSON.stringify({ name: 'app', component: 'app.wasm', extensions: packages }));
    await assert.rejects(loadConfig(path), /installed npm package names/);
  }
  await assert.rejects(resolveExtensions(f.config), /Cannot resolve/);
  const config = { ...f.config, build: { watch: ['src'] } };
  assert.equal(shouldRebuild(config, 'package-lock.json'), true);
  assert.equal(shouldRebuild(config, 'package.json'), true);
  assert.equal(shouldRebuild(config, 'node_modules/example/value.mjs'), false);
});
