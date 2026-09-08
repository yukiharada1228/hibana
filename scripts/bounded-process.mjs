import {spawn} from 'node:child_process';

// No shell. Terminate and reap the process group before reporting cancellation.
export function runCommand(executable, args, {timeoutMs = 30000, signal, maxBytes = 8 * 1024 * 1024, input, cwd, env} = {}) {
  signal?.throwIfAborted();
  return new Promise((resolve, reject) => {
    const child = spawn(executable, args, {cwd, env, detached: process.platform !== 'win32', stdio: [input === undefined ? 'ignore' : 'pipe', 'pipe', 'pipe']});
    child.stdin?.on('error', () => {}); // An early rejection may close stdin; exit status remains authoritative.
    child.stdin?.end(input);
    let size = 0, chunks = [], failure, closed = false, killed = false, killTimer;
    const kill = sig => {
      if (!child.pid) return;
      try { process.platform === 'win32' ? child.kill(sig) : process.kill(-child.pid, sig); }
      catch (error) { if (error.code !== 'ESRCH') failure ||= error; }
    };
    const finish = () => {
      if (!closed || (killTimer && !killed)) return;
      clearTimeout(timer);
      clearTimeout(killTimer);
      signal?.removeEventListener('abort', abort);
      failure ? reject(failure) : resolve(Buffer.concat(chunks).toString());
    };
    const cancel = error => {
      if (killTimer) return;
      failure = error;
      kill('SIGTERM');
      killTimer = setTimeout(() => { kill('SIGKILL'); killed = true; finish(); }, 250);
    };
    const abort = () => cancel(signal.reason || new Error('Command canceled'));
    const timer = setTimeout(() => cancel(new Error('Command deadline exceeded')), timeoutMs);
    signal?.addEventListener('abort', abort, {once: true});
    if (signal?.aborted) abort();
    child.stdout.on('data', chunk => {
      size += chunk.length;
      if (size > maxBytes) cancel(new Error('Command output limit exceeded'));
      else chunks.push(chunk);
    });
    child.stderr.on('data', chunk => {
      size += chunk.length;
      if (size > maxBytes) cancel(new Error('Command output limit exceeded'));
    });
    child.on('error', error => { failure ||= error; });
    child.on('close', (code, sig) => {
      closed = true;
      if (code !== 0) failure ||= new Error(`Command failed (${sig || code})`);
      finish();
    });
  });
}
