import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  rm,
  stat,
  readdir,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { setTimeout as sleep } from "node:timers/promises";

const connection = { url: "https://fixture.invalid/api", token: "test-only" };
const worker = `
  import {saveProfile, profileCommand, logout} from ${JSON.stringify(new URL("../src/profiles.mjs", import.meta.url).href)};
  process.once('message', async () => {
    const [operation, name] = JSON.parse(process.argv[1]);
    process.send('started');
    try {
      if (operation === 'save') await saveProfile(name, ${JSON.stringify(connection)});
      else if (operation === 'logout') await logout({profile:name});
      else await profileCommand([operation, name]);
    } catch (error) {
      console.error(error.message);
      process.exitCode = 1;
    } finally { process.disconnect(); }
  });
  process.send('ready');
`;

async function fixture(t, initial = { profiles: {} }) {
  const home = await mkdtemp(join(tmpdir(), "hibana-profiles-"));
  const file = join(home, "profiles.json");
  const lock = `${file}.lock`;
  await writeFile(file, JSON.stringify(initial), { mode: 0o600 });
  const children = [];
  t.after(async () => {
    for (const { child } of children) if (child.exitCode === null) child.kill();
    await Promise.all(children.map(({ done }) => done));
    await rm(home, { recursive: true, force: true });
  });
  const launch = (operation, name) => {
    const child = spawn(
      process.execPath,
      [
        "--input-type=module",
        "--eval",
        worker,
        JSON.stringify([operation, name]),
      ],
      {
        env: { ...process.env, HIBANA_CONFIG_HOME: home, HIBANA_PROFILE: "" },
        stdio: ["ignore", "pipe", "pipe", "ipc"],
        timeout: 15000,
      },
    );
    let output = "";
    child.stdout.on("data", (chunk) => (output += chunk));
    child.stderr.on("data", (chunk) => (output += chunk));
    const messages = new Set();
    const waiters = new Map();
    child.on("message", (message) => {
      messages.add(message);
      waiters.get(message)?.();
    });
    const done = new Promise((resolve) => {
      child.once("error", (error) =>
        resolve({ code: -1, output: error.message }),
      );
      child.once("close", (code) => resolve({ code, output }));
    });
    const wait = (message) =>
      Promise.race([
        messages.has(message)
          ? Promise.resolve()
          : new Promise((resolve) => waiters.set(message, resolve)),
        done.then((result) => {
          throw new Error(
            `Profile process exited before ${message}: ${result.output}`,
          );
        }),
      ]);
    const start = async () => {
      await wait("ready");
      child.send("go");
      await wait("started");
    };
    const result = { child, done, start };
    children.push(result);
    return result;
  };
  return {
    home,
    file,
    lock,
    launch,
    read: async () => JSON.parse(await readFile(file, "utf8")),
  };
}

test("parallel profile commands preserve saves, removals and logout across processes", async (t) => {
  const f = await fixture(t, {
    current: "stay",
    profiles: { stay: connection, leave: connection },
  });
  const operations = Array.from({ length: 12 }, (_, i) =>
    f.launch("save", `profile-${i}`),
  );
  operations.push(
    f.launch("logout", "stay"),
    f.launch("remove", "leave"),
    f.launch("use", "stay"),
  );
  await Promise.all(operations.map((operation) => operation.start()));
  for (const result of await Promise.all(
    operations.map((operation) => operation.done),
  ))
    assert.equal(result.code, 0, result.output);
  const state = await f.read();
  for (let i = 0; i < 12; i++)
    assert.deepEqual(state.profiles[`profile-${i}`], connection);
  assert.equal(
    state.profiles.stay.token,
    undefined,
    "another save must never resurrect a logged-out token",
  );
  assert.equal(Object.hasOwn(state.profiles, "leave"), false);
  assert.ok(Object.hasOwn(state.profiles, state.current));
  assert.equal((await stat(f.home)).mode & 0o777, 0o700);
  assert.equal((await stat(f.file)).mode & 0o777, 0o600);
  assert.deepEqual(await readdir(f.home), ["profiles.json"]);
});

test("every profile mutation reads the state only after acquiring the process lock", async (t) => {
  for (const operation of ["save", "logout", "remove", "use"]) {
    await t.test(operation, async (t) => {
      const initial = { current: "target", profiles: { target: connection } };
      const f = await fixture(t, initial);
      await mkdir(f.lock, { mode: 0o700 });
      const pending = f.launch(
        operation,
        operation === "save" ? "new" : "target",
      );
      await pending.start();
      // Model a preceding writer still owning the lock, then committing a new
      // profile before this command can read. Readers may keep reading meanwhile.
      await sleep(150);
      assert.equal(
        pending.child.exitCode,
        null,
        "mutation must wait for the lock",
      );
      assert.deepEqual(await f.read(), initial);
      await writeFile(
        f.file,
        JSON.stringify({
          ...initial,
          profiles: { ...initial.profiles, preceding: connection },
        }),
      );
      await rm(f.lock, { recursive: true });
      const result = await pending.done;
      assert.equal(result.code, 0, result.output);
      assert.deepEqual((await f.read()).profiles.preceding, connection);
    });
  }
});

test("a busy profile lock times out without stealing it or changing the file", async (t) => {
  const initial = { profiles: { target: connection } };
  const f = await fixture(t, initial);
  await mkdir(f.lock, { mode: 0o700 });
  const pending = f.launch("logout", "target");
  await pending.start();
  const result = await pending.done;
  assert.equal(result.code, 1);
  assert.match(result.output, /Profiles are busy/);
  assert.match(result.output, /confirming no profile command is running/);
  assert.deepEqual(await f.read(), initial);
  assert.ok((await stat(f.lock)).isDirectory());
});

test("failed profile operations release the lock and preserve stored credentials", async (t) => {
  const initial = { profiles: { target: connection } };
  const f = await fixture(t, initial);
  for (const operation of ["logout", "remove", "use"]) {
    const pending = f.launch(operation, "missing");
    await pending.start();
    assert.equal((await pending.done).code, 1);
    assert.deepEqual(await f.read(), initial);
    await assert.rejects(stat(f.lock), { code: "ENOENT" });
  }
  await writeFile(f.file, "invalid-json");
  const invalid = f.launch("save", "new");
  await invalid.start();
  assert.equal((await invalid.done).code, 1);
  assert.equal(await readFile(f.file, "utf8"), "invalid-json");
  await assert.rejects(stat(f.lock), { code: "ENOENT" });
});
