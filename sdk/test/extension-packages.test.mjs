import test from 'node:test';
import assert from 'node:assert/strict';
import {execFile,fork} from 'node:child_process';
import {createHash} from 'node:crypto';
import {once} from 'node:events';
import {createServer} from 'node:https';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {mkdtemp,mkdir,writeFile,readFile,rm,readdir,symlink} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,dirname} from 'node:path';
import {build} from 'esbuild';
import {resolveExtensions} from '../src/extensions.mjs';
import {extensionImports} from '../src/extension-imports.mjs';
import {validateExtensionList,HTTP_CONTRACT} from '../src/extension-manifest.mjs';
import {shouldRebuild} from '../src/dev.mjs';
import {parseCommand} from '../src/commands.mjs';
import {installExtensionPackages} from '../src/extension-packages.mjs';

const exec=promisify(execFile);
const json=value=>JSON.stringify(value,null,2)+'\n';
async function fixture(t, manifestFields = {}) {
  const root=await mkdtemp(join(tmpdir(),'hibana-managed-extensions-'));
  const previous=Object.fromEntries(['npm_config_cache','npm_config_offline','npm_config_registry','npm_config_strict_ssl'].map(name=>[name,process.env[name]]));
  process.env.npm_config_cache=join(root,'empty-npm-cache');
  process.env.npm_config_offline='true';
  t.after(async()=>{
    for(const [name,value] of Object.entries(previous)) {
      if(value===undefined)delete process.env[name];else process.env[name]=value;
    }
    await rm(root,{recursive:true,force:true});
  });
  const publisher=join(root,'publisher'), vendor=join(root,'vendor');
  await mkdir(join(publisher,'node_modules/@fixture/shared'),{recursive:true});await mkdir(vendor);
  const hook=`node -e "require('node:fs').writeFileSync('HOOK-RAN','bad');process.exit(1)"`;
  await writeFile(join(publisher,'package.json'),json({name:'@fixture/extension',version:'1.0.0',type:'module',exports:{'.':'./index.mjs','./subpath':'./index.mjs','./hibana.extension.json':'./hibana.extension.json'},dependencies:{'@fixture/shared':'1.0.0'},bundleDependencies:['@fixture/shared'],scripts:{preinstall:hook,install:hook,postinstall:hook}}));
  await writeFile(join(publisher,'index.mjs'),'export {value} from "@fixture/shared";');
  await writeFile(join(publisher,'hibana.extension.json'),json({schemaVersion:2,runtime:HTTP_CONTRACT,dependencies:['@fixture/shared'],aliases:{'node:fixture':'./index.mjs'},...manifestFields}));
  const shared=join(publisher,'node_modules/@fixture/shared');
  await writeFile(join(shared,'package.json'),json({name:'@fixture/shared',version:'1.0.0',type:'module',exports:{'.':'./index.mjs','./value':'./index.mjs','./hibana.extension.json':'./hibana.extension.json'},scripts:{install:hook}}));
  await writeFile(join(shared,'index.mjs'),'export const value = "managed";');
  await writeFile(join(shared,'hibana.extension.json'),json({schemaVersion:2,runtime:HTTP_CONTRACT}));
  const packed=JSON.parse((await exec('npm',['pack','--ignore-scripts','--json','--pack-destination',vendor],{cwd:publisher})).stdout)[0];
  await writeFile(join(root,'package.json'),json({private:true,dependencies:{ordinary:'1.0.0'}}));
  await writeFile(join(root,'package-lock.json'),'ordinary app lock is not owned by Hibana\n');
  await mkdir(join(root,'node_modules/ordinary'),{recursive:true});
  await writeFile(join(root,'node_modules/ordinary/package.json'),json({name:'ordinary',version:'1.0.0',main:'index.js'}));
  await writeFile(join(root,'node_modules/ordinary/index.js'),'exports.value="application";');
  return {root,archive:join(vendor,packed.filename),config:{root,path:join(root,'hibana.json'),main:'index.mjs',extensions:{'@fixture/extension':'./vendor/'+packed.filename}}};
}

test('managed sources are explicit and frozen mode is exposed only on build/dev/deploy',()=>{
  for(const source of ['1.2.3','1.2.3-rc.1','1.2.3+build.01','1.2.3-0.alpha+linux','./vendor/extension.tgz','https://example.com/pkg.tgz'])assert.doesNotThrow(()=>validateExtensionList({'@fixture/extension':source}));
  assert.doesNotThrow(()=>validateExtensionList({}));
  for(const source of ['latest','^1.0.0','1.2.3-01','1.2.3-rc.01','9007199254740992.0.0','v1.2.3','01.2.3','1.2.3+','1.2.3+build..1','../pkg.tgz','/tmp/pkg.tgz','file:./pkg.tgz','./a/../pkg.tgz','https://user:pass@example.com/pkg.tgz','https://example.com/pkg.tgz?token=x','http://example.com/pkg.tgz','git+https://example.com/a.git',null])
    assert.throws(()=>validateExtensionList({'@fixture/extension':source}));
  for(const command of ['build','dev','deploy'])assert.equal(parseCommand([command,'--frozen-lockfile']).values['frozen-lockfile'],true);
  assert.throws(()=>parseCommand(['rollback','--frozen-lockfile']));
});

test('hibana.json alone installs extensions, restores a portable lock, and resolves imports without changing app npm files',async t=>{
  const f=await fixture(t), snapshots=new Map();
  for(const file of ['package.json','package-lock.json','node_modules/ordinary/index.js'])snapshots.set(file,await readFile(join(f.root,file)));
  const plan=await resolveExtensions(f.config);
  assert.deepEqual(plan.metadata.extensions.map(e=>e.name),['@fixture/shared','@fixture/extension']);
  const lock=await readFile(join(f.root,'hibana-lock.json'),'utf8');
  assert.ok(!lock.includes(f.root));
  assert.match(lock, /sha512-/);
  for(const [file,bytes] of snapshots)assert.deepEqual(await readFile(join(f.root,file)),bytes);
  assert.deepEqual(await readdir(join(f.root,'node_modules')),['ordinary']);
  for(const root of Object.values(plan.packages))await assert.rejects(readFile(join(root,'HOOK-RAN')),{code:'ENOENT'});
  const compiled=await build({stdin:{contents:'import {value as root} from "@fixture/extension/subpath";import {value as child} from "@fixture/shared/value";import {value as alias} from "node:fixture";import {value as normal} from "ordinary";export default {root,child,alias,normal};',resolveDir:f.root},absWorkingDir:f.root,bundle:true,platform:'browser',format:'esm',write:false,alias:plan.aliases,plugins:[extensionImports(plan.packages)]});
  const result=await import('data:text/javascript;base64,'+Buffer.from(compiled.outputFiles[0].text).toString('base64'));
  assert.deepEqual(result.default,{root:'managed',child:'managed',alias:'managed',normal:'application'});
  const warm=await resolveExtensions(f.config,{frozenLockfile:true});assert.deepEqual(warm.metadata,plan.metadata);
  await rm(join(f.root,'.hibana/extensions'),{recursive:true});
  await rm(join(f.root,'empty-npm-cache'),{recursive:true,force:true});
  const restored=await resolveExtensions(f.config,{frozenLockfile:true});
  assert.deepEqual(restored.metadata,plan.metadata);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),lock);
  const moved=join(f.root,'moved');await mkdir(join(moved,'vendor'),{recursive:true});
  const archiveName=f.config.extensions['@fixture/extension'].slice('./vendor/'.length);
  await writeFile(join(moved,'vendor',archiveName),await readFile(f.archive));await writeFile(join(moved,'hibana-lock.json'),lock);
  assert.deepEqual((await resolveExtensions({...f.config,root:moved},{frozenLockfile:true})).metadata,plan.metadata);
  assert.equal(shouldRebuild({...f.config,build:{watch:['src']}},'hibana-lock.json'),true);
  assert.equal(shouldRebuild({...f.config,build:{watch:['src']}},'vendor/'+archiveName),true);
  assert.equal(shouldRebuild(f.config,'.hibana/extensions/staging-abc/package.json'),false);
});

test('locked archive replacement, missing locks, changed sources and invalid locks fail before replacing working state',async t=>{
  const f=await fixture(t);
  await assert.rejects(resolveExtensions(f.config,{frozenLockfile:true}),/hibana-lock.json is missing/);
  await resolveExtensions(f.config);
  const lock=await readFile(join(f.root,'hibana-lock.json'),'utf8'), bytes=await readFile(f.archive);
  await writeFile(f.archive,Buffer.concat([bytes,Buffer.from('changed')]));
  await assert.rejects(resolveExtensions(f.config),/integrity differs/);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),lock);
  await writeFile(f.archive,bytes);
  await writeFile(join(f.root,'vendor/broken.tgz'),'not a tarball');
  const changed={...f.config,extensions:{'@fixture/extension':'./vendor/broken.tgz'}};
  await assert.rejects(resolveExtensions(changed,{frozenLockfile:true}),/differs from hibana.json/);
  await assert.rejects(resolveExtensions(changed),/Could not install Hibana extensions/);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),lock);
  const invalid=JSON.parse(lock);delete invalid.npm.packages['node_modules/@fixture/extension'].integrity;
  await writeFile(join(f.root,'hibana-lock.json'),json(invalid));
  await assert.rejects(resolveExtensions(f.config),/Invalid hibana-lock.json/);
  // Changing sources must not let an invalid previous resolution seed npm.
  await writeFile(join(f.root,'vendor/new-source.tgz'),bytes);
  await assert.rejects(resolveExtensions({...f.config,extensions:{'@fixture/extension':'./vendor/new-source.tgz'}}),/Invalid hibana-lock.json/);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),json(invalid));
});

test('concurrent cold installs share a complete cache and a consistent lock',async t=>{
  const f=await fixture(t);
  const plans=await Promise.all([resolveExtensions(f.config),resolveExtensions(f.config)]);
  assert.deepEqual(plans[0],plans[1]);
  const folders=await readdir(join(f.root,'.hibana/extensions'));
  assert.equal(folders.length,1);
  assert.match(folders[0],/^[a-f0-9]{64}$/);
  await resolveExtensions(f.config,{frozenLockfile:true});
});

test('different CLI processes cannot overwrite a competing lock update and release the guard after rejection',async t=>{
  const f=await fixture(t);
  await resolveExtensions(f.config);
  const original=await readFile(join(f.root,'hibana-lock.json'),'utf8');
  const configs=[];
  for(const name of ['one','two']){
    await writeFile(join(f.root,'vendor',name+'.tgz'),await readFile(f.archive));
    configs.push({...f.config,extensions:{'@fixture/extension':`./vendor/${name}.tgz`}});
  }
  const children=configs.map(config=>{
    const child=fork(new URL('./fixtures/extension-commit.mjs',import.meta.url),[JSON.stringify(config)],{silent:true});
    t.after(()=>{if(child.exitCode===null)child.kill('SIGKILL');});
    let stderr='';child.stderr.on('data',bytes=>stderr+=bytes);
    return {child,ready:once(child,'message',{signal:AbortSignal.timeout(15000)}),closed:once(child,'close'),stderr:()=>stderr};
  });
  for(const child of children)assert.equal((await child.ready)[0],'ready');
  const guard=join(f.root,'.hibana/lock-update');
  await mkdir(guard);
  const committing=children.map(({child})=>once(child,'message',{signal:AbortSignal.timeout(15000)}));
  for(const {child} of children)child.send('commit');
  for(const pending of committing)assert.equal((await pending)[0],'committing');
  await delay(100);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),original);
  await rm(guard,{recursive:true});
  const exits=await Promise.all(children.map(child=>child.closed));
  assert.deepEqual(exits.map(([code])=>code).sort(),[0,1]);
  const winner=exits.findIndex(([code])=>code===0), loser=1-winner;
  assert.match(children[loser].stderr(),/changed during extension installation/);
  assert.deepEqual(JSON.parse(await readFile(join(f.root,'hibana-lock.json'))).sources,configs[winner].extensions);
  await assert.rejects(readdir(guard),{code:'ENOENT'});
  await resolveExtensions(configs[winner],{frozenLockfile:true});
  await resolveExtensions(configs[loser]);
  await resolveExtensions(configs[loser],{frozenLockfile:true});
});

test('an abandoned publication guard times out without overwriting the lock or stealing the guard',async t=>{
  const f=await fixture(t), prepared=await installExtensionPackages(f.config);
  const guard=join(f.root,'.hibana/lock-update');await mkdir(guard);
  await assert.rejects(prepared.commitLock(),/Timed out.*remove \.hibana\/lock-update/);
  assert.deepEqual(await readdir(guard),[]);
  await assert.rejects(readFile(join(f.root,'hibana-lock.json')),{code:'ENOENT'});
  await rm(guard,{recursive:true});
  await prepared.commitLock();
  await resolveExtensions(f.config,{frozenLockfile:true});
});

test('adding and removing independent roots preserves ranged dependencies while explicit upgrades still work',async t=>{
  const f=await fixture(t), vendor=join(f.root,'vendor');
  async function pack(name,version,dependencies={}){
    const directory=join(f.root,`${name}-${version}`);await mkdir(directory);
    await writeFile(join(directory,'package.json'),json({name:`@fixture/${name}`,version,dependencies,main:'index.js'}));
    await writeFile(join(directory,'index.js'),`exports.version = ${JSON.stringify(version)};`);
    if(name==='ranged')await writeFile(join(directory,'hibana.extension.json'),json({schemaVersion:2,runtime:HTTP_CONTRACT}));
    return JSON.parse((await exec('npm',['pack','--ignore-scripts','--json','--pack-destination',vendor],{cwd:directory})).stdout)[0];
  }
  const a=await pack('ranged','1.0.0',{'@fixture/leaf':'^1.0.0'});
  const upgraded=await pack('ranged','2.0.0',{'@fixture/leaf':'^2.0.0'});
  const archives=new Map();
  for(const version of ['1.0.0','1.1.0','2.0.0']){
    const packed=await pack('leaf',version);
    archives.set(version,await readFile(join(vendor,packed.filename)));
  }
  const key=join(f.root,'key.pem'),cert=join(f.root,'cert.pem');
  await exec('openssl',['req','-x509','-newkey','rsa:2048','-nodes','-keyout',key,'-out',cert,'-subj','/CN=localhost','-days','1']);
  let available=['1.0.0'],registry;
  const server=createServer({key:await readFile(key),cert:await readFile(cert)},(req,res)=>{
    res.setHeader('cache-control','no-store');
    if(decodeURIComponent(req.url)==='/@fixture/leaf'){
      const versions=Object.fromEntries(available.map(version=>[version,{name:'@fixture/leaf',version,dist:{tarball:`${registry}leaf-${version}.tgz`,integrity:'sha512-'+createHash('sha512').update(archives.get(version)).digest('base64')}}]));
      res.setHeader('content-type','application/json');
      res.end(json({name:'@fixture/leaf','dist-tags':{latest:available.at(-1)},versions}));
    }else{
      const bytes=archives.get(req.url.match(/^\/leaf-(.+)\.tgz$/)?.[1]);
      if(bytes)res.end(bytes);else{res.statusCode=404;res.end();}
    }
  });
  server.listen(0,'127.0.0.1');await once(server,'listening');
  t.after(()=>new Promise(resolve=>server.close(resolve)));
  registry=`https://127.0.0.1:${server.address().port}/`;
  process.env.npm_config_registry=registry;
  process.env.npm_config_strict_ssl='false';
  process.env.npm_config_offline='false';
  const config={...f.config,extensions:{'@fixture/ranged':'./vendor/'+a.filename}};
  const lockedVersion=async()=>JSON.parse(await readFile(join(f.root,'hibana-lock.json'))).npm.packages['node_modules/@fixture/leaf'].version;
  await resolveExtensions(config);assert.equal(await lockedVersion(),'1.0.0');
  available=['1.0.0','1.1.0','2.0.0'];
  const added={...config,extensions:{...config.extensions,...f.config.extensions}};
  await resolveExtensions(added);assert.equal(await lockedVersion(),'1.0.0');
  await resolveExtensions(added,{frozenLockfile:true});
  await rm(f.archive); // Removed roots must not require their old source archives.
  await resolveExtensions(config);assert.equal(await lockedVersion(),'1.0.0');
  config.extensions['@fixture/ranged']='./vendor/'+upgraded.filename;
  await rm(join(vendor,a.filename));
  await resolveExtensions(config);assert.equal(await lockedVersion(),'2.0.0');
  await rm(join(f.root,'.hibana/extensions'),{recursive:true});
  await resolveExtensions(config,{frozenLockfile:true});assert.equal(await lockedVersion(),'2.0.0');
});

test('archive paths cannot leave the project and archive names must match the declared extension',async t=>{
  const f=await fixture(t);const app=join(f.root,'other');await mkdir(app);
  await symlink(f.archive,join(app,'outside.tgz'));
  await assert.rejects(resolveExtensions({...f.config,root:app,extensions:{'@fixture/extension':'./outside.tgz'}}),/stay inside the project/);
  await assert.rejects(resolveExtensions({...f.config,extensions:{'@fixture/wrong':f.config.extensions['@fixture/extension']}}),/package name does not match/);
  await assert.rejects(readFile(join(f.root,'hibana-lock.json')),{code:'ENOENT'});
});

test('invalid extension graphs cannot publish a lockfile',async t=>{
  const f=await fixture(t,{runtime:'unsupported:runtime/handler@1.0.0'});
  await assert.rejects(resolveExtensions(f.config),/Incompatible runtime contract/);
  await assert.rejects(readFile(join(f.root,'hibana-lock.json')),{code:'ENOENT'});
});

test('ordinary imports named like Object prototype members keep normal resolution',async t=>{
  const f=await fixture(t);
  const directory=join(f.root,'node_modules/constructor');await mkdir(directory);
  await writeFile(join(directory,'package.json'),json({name:'constructor',version:'1.0.0',main:'index.js'}));
  await writeFile(join(directory,'index.js'),'exports.value=42;');
  const output=await build({stdin:{contents:'export {value} from "constructor";',resolveDir:f.root},bundle:true,platform:'browser',format:'esm',write:false,plugins:[extensionImports()]});
  assert.equal((await import('data:text/javascript;base64,'+Buffer.from(output.outputFiles[0].text).toString('base64'))).value,42);
});

test('separate archives in hibana.json share one dependency installation',async t=>{
  const f=await fixture(t), publisher=join(f.root,'publisher'), vendor=join(f.root,'vendor');
  const pkg=JSON.parse(await readFile(join(publisher,'package.json')));
  delete pkg.bundleDependencies;
  await writeFile(join(publisher,'package.json'),json(pkg));
  const shared=JSON.parse((await exec('npm',['pack','--ignore-scripts','--json','--pack-destination',vendor],{cwd:join(publisher,'node_modules/@fixture/shared')})).stdout)[0];
  await exec('npm',['pack','--ignore-scripts','--pack-destination',vendor],{cwd:publisher});
  f.config.extensions['@fixture/shared']='./vendor/'+shared.filename;
  const plan=await resolveExtensions(f.config);
  assert.deepEqual(plan.metadata.extensions.map(e=>e.name),['@fixture/shared','@fixture/extension']);
  assert.deepEqual(plan.metadata.roots,['@fixture/extension','@fixture/shared']);
  assert.equal(plan.packages['@fixture/shared'],join(plan.packages['@fixture/extension'],'../shared'));
  await rm(join(f.root,'.hibana'),{recursive:true});
  const restored=await resolveExtensions(f.config,{frozenLockfile:true});
  assert.deepEqual(restored.metadata,plan.metadata);
});

test('adding or removing another extension never accepts replacement of an unchanged archive source',async t=>{
  const f=await fixture(t);
  const publisher=join(f.root,'extra');await mkdir(publisher);
  await writeFile(join(publisher,'package.json'),json({name:'@fixture/extra',version:'1.0.0'}));
  await writeFile(join(publisher,'hibana.extension.json'),json({schemaVersion:2,runtime:HTTP_CONTRACT}));
  const packed=JSON.parse((await exec('npm',['pack','--ignore-scripts','--json','--pack-destination',join(f.root,'vendor')],{cwd:publisher})).stdout)[0];
  const added={...f.config,extensions:{...f.config.extensions,'@fixture/extra':'./vendor/'+packed.filename}};
  await resolveExtensions(f.config);
  const original=await readFile(f.archive);
  await writeFile(join(f.root,'publisher/index.mjs'),'export {value} from "@fixture/shared"; // replaced at the same source\n');
  await exec('npm',['pack','--ignore-scripts','--pack-destination',join(f.root,'vendor')],{cwd:join(f.root,'publisher')});
  const replacement=await readFile(f.archive);
  assert.notDeepEqual(replacement,original);
  const originalLock=await readFile(join(f.root,'hibana-lock.json'),'utf8');
  await writeFile(f.archive,replacement);
  await assert.rejects(resolveExtensions(added),/integrity differs/);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),originalLock);
  await writeFile(f.archive,original);
  await resolveExtensions(added);
  const expandedLock=await readFile(join(f.root,'hibana-lock.json'),'utf8');
  await writeFile(f.archive,replacement);
  await assert.rejects(resolveExtensions(f.config),/integrity differs/);
  assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),expandedLock);
  await writeFile(join(f.root,'vendor/updated.tgz'),replacement);
  const updated={...f.config,extensions:{'@fixture/extension':'./vendor/updated.tgz'}};
  await resolveExtensions(updated);
  await resolveExtensions(updated,{frozenLockfile:true});
});

test('removing all extensions requires a lock update, including an omitted field or empty legacy array',async t=>{
  const f=await fixture(t);
  for(const extensions of [{},undefined,[]]){
    await resolveExtensions(f.config);
    const previous=await readFile(join(f.root,'hibana-lock.json'),'utf8');
    const empty={...f.config,extensions};
    await assert.rejects(resolveExtensions(empty,{frozenLockfile:true}),/differs from hibana.json/);
    assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),previous);
    assert.deepEqual((await resolveExtensions(empty)).metadata.extensions,[]);
    const lock=JSON.parse(await readFile(join(f.root,'hibana-lock.json'),'utf8'));
    assert.deepEqual(lock.sources,{});assert.deepEqual(lock.integrity,{});
    assert.deepEqual(Object.keys(lock.npm.packages),['']);
    await resolveExtensions(empty,{frozenLockfile:true});
  }
  await rm(join(f.root,'hibana-lock.json'));
  await rm(join(f.root,'.hibana'),{recursive:true});
  for(const extensions of [{},undefined,[]])await resolveExtensions({...f.config,extensions},{frozenLockfile:true});
  await assert.rejects(readFile(join(f.root,'hibana-lock.json')),{code:'ENOENT'});
  await assert.rejects(readdir(join(f.root,'.hibana')),{code:'ENOENT'});
});

test('partial caches restore atomically from the unchanged lock, including concurrent repairs',async t=>{
  const f=await fixture(t);
  const original=await resolveExtensions(f.config);
  const lock=await readFile(join(f.root,'hibana-lock.json'),'utf8');
  const cache=dirname(dirname(dirname(original.packages['@fixture/extension'])));
  const source=join(original.packages['@fixture/shared'],'index.mjs');
  const content=await readFile(source);
  const damage=[
    ()=>rm(join(cache,'node_modules'),{recursive:true}),
    ()=>rm(original.packages['@fixture/shared'],{recursive:true}),
    ()=>rm(source),
    ()=>writeFile(source,''),
    ()=>rm(join(cache,'ready')),
    ()=>writeFile(join(cache,'ready'),'invalid marker'),
    ()=>writeFile(join(cache,'ready'),cache.split('/').at(-1)),
    ()=>writeFile(join(cache,'ready'),json({key:cache.split('/').at(-1),files:[['\0',1]]})),
  ];
  for(const corrupt of damage){
    await corrupt();
    const restored=await Promise.all([resolveExtensions(f.config,{frozenLockfile:true}),resolveExtensions(f.config,{frozenLockfile:true})]);
    for(const plan of restored)assert.deepEqual(plan,original);
    assert.deepEqual(await readFile(source),content);
    assert.equal(await readFile(join(f.root,'hibana-lock.json'),'utf8'),lock);
    assert.deepEqual(await readdir(join(f.root,'.hibana/extensions')),[cache.split('/').at(-1)]);
  }
});
