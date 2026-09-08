import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {runCommand} from './bounded-process.mjs';

for (const cancellation of ['timeout', 'abort']) {
  test(`${cancellation} kills a hung process tree and permits independent cleanup`, async () => {
    const folder = await mkdtemp(join(tmpdir(), 'hibana-process-'));
    try {
      const marker = join(folder, 'ticks');
      const child = `process.on('SIGTERM',()=>{}); const fs=require('fs'); setInterval(()=>fs.appendFileSync(${JSON.stringify(marker)},'x'),10);`;
      const parent = `process.on('SIGTERM',()=>{}); require('child_process').spawn(process.execPath,['-e',${JSON.stringify(child)}],{stdio:'inherit'}); setInterval(()=>{},1000);`;
      const controller = new AbortController();
      const started = Date.now();
      const task = runCommand(process.execPath, ['-e', parent], {timeoutMs: cancellation === 'timeout' ? 500 : 5000, signal: controller.signal});
      const timer = cancellation === 'abort' ? setTimeout(() => controller.abort(new Error('fixture abort')), 500) : undefined;
      await assert.rejects(task, /deadline|fixture abort/);
      clearTimeout(timer);
      assert.ok(Date.now() - started < 3000);
      const before = await readFile(marker, 'utf8');
      assert.ok(before.length > 0, 'descendant actually ran');
      await delay(150);
      assert.equal(await readFile(marker, 'utf8'), before, 'descendant stopped before rejection');
      assert.equal(await runCommand(process.execPath, ['-e', 'process.stdout.write("cleanup")'], {timeoutMs: 1000}), 'cleanup');
    } finally { await rm(folder, {recursive: true, force: true}); }
  });
}
test('unbounded output is canceled', async () => {
  await assert.rejects(runCommand(process.execPath, ['-e', 'setInterval(()=>process.stdout.write("x".repeat(65536)),1)'], {maxBytes: 1024}), /output limit/);
});
