import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {setTimeout as sleep} from 'node:timers/promises';

// Keep bounded evidence, not the full two-hour stream or any credentials.
export async function startTail({cliEntry, cwd, env, secrets}) {
  const child=spawn(process.execPath,[cliEntry,'tail','inventory-api','--format','json'],{cwd,env,stdio:['ignore','pipe','pipe']});
  let connected=false, stopping=false, closed=false, exitCode, failure, buffered='', stderr='', count=0, lastEvent=0;
  const recent=[], seen=new Set(), statuses={};
  child.on('error',()=>{failure='CLI tail failed to start';});
  const exited=new Promise(resolve=>child.on('close',code=>{closed=true;exitCode=code;if(!stopping) failure='CLI tail exited during load';resolve();}));
  const protect=value=>assert.ok(!secrets.some(secret=>secret&&value.includes(secret)),'Secret found in CLI output');
  child.stderr.on('data',chunk=>{
    try {
      stderr+=chunk.toString();protect(stderr);
      assert.ok(stderr.length<=16384,'Unexpected tail diagnostic volume');
      if(stderr.includes('Connected to inventory-api.')) connected=true;
      assert.ok(!/Warning:|connection lost|reconnected|Error:/i.test(stderr),'Tail stream interrupted or dropped events');
    } catch {failure='CLI tail diagnostics failed validation';}
  });
  child.stdout.setEncoding('utf8');
  child.stdout.on('data',chunk=>{
    try {
      buffered+=chunk;assert.ok(buffered.length<1024*1024,'Oversized tail output');
      for(let end;(end=buffered.indexOf('\n'))>=0;) {
        const line=buffered.slice(0,end);buffered=buffered.slice(end+1);protect(line);
        const item=JSON.parse(line);
        assert.equal(item.status,'succeeded');assert.ok([200,404].includes(item.http_status));
        assert.equal(item.logs?.truncated,false);assert.equal(item.logs.stderr,'');
        const logs=item.logs.stdout.trim().split('\n').map(JSON.parse);
        assert.ok(logs.some(log=>log.event==='acceptance_request'&&log.status===item.http_status&&log.release==='v2'));
        assert.ok(!seen.has(item.execution_id),'Duplicate tail execution');
        seen.add(item.execution_id);if(seen.size>1000) seen.delete(seen.values().next().value);
        count++;lastEvent=Date.now();statuses[item.http_status]=(statuses[item.http_status]||0)+1;
        recent.push({execution_id:item.execution_id,logs:item.logs});if(recent.length>10) recent.shift();
      }
    } catch {failure='CLI tail event failed JSON/log/provenance validation';}
  });
  const check=()=>{if(failure) throw new Error(failure);};
  const stop=async()=>{
    stopping=true;const began=Date.now();child.kill('SIGINT');
    const timer=setTimeout(()=>child.kill('SIGKILL'),5000);
    try {await exited;} finally {clearTimeout(timer);}
    check();assert.equal(exitCode,0,'Tail did not stop cleanly');
    assert.ok(Date.now()-began<5000,'Tail shutdown exceeded deadline');
  };
  try {
    const deadline=Date.now()+30000;
    while(!connected&&!closed&&Date.now()<deadline) {check();await sleep(100);}
    check();assert.ok(connected,'Tail did not connect');
  } catch(error) {await stop().catch(()=>{});throw error;}
  return {
    stop,
    snapshot() {check();assert.ok(lastEvent&&Date.now()-lastEvent<15000,'No recent live events during HTTP load');return {count,statuses:{...statuses}};},
    async verifyStored(api) {
      check();assert.ok(recent.length>0,'Tail returned no executions');
      for(const item of [...recent]) {
        const detail=await api(`/executions/${encodeURIComponent(item.execution_id)}`);
        assert.ok(['stdout','stderr','truncated'].every(key=>detail.logs?.[key]===item.logs[key]),'Saved logs differ from live tail');
      }
      return {passed:true,events:count,statuses:{...statuses},saved_logs_verified:recent.length};
    },
  };
}
