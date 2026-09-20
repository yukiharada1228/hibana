// A selected Cargo build does not fetch every workspace member's dependencies.
// License metadata must still work with a fresh registry cache and lockfile.
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  rm,
  symlink,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildComponent } from "../extensions/build-component.mjs";
import { runCommand } from "./bounded-process.mjs";

const folder = await mkdtemp(join(tmpdir(), "hibana-cold-extension-"));
const previousCargoHome = process.env.CARGO_HOME;
process.env.CARGO_HOME = join(folder, "cargo-home");
try {
  const root = join(folder, "extension");
  await mkdir(join(root, "component/src"), { recursive: true });
  await mkdir(join(root, "component/wit"), { recursive: true });
  await mkdir(join(folder, "unrelated/src"), { recursive: true });
  await writeFile(
    join(folder, "Cargo.toml"),
    '[workspace]\nresolver = "2"\nmembers = ["extension/component", "unrelated"]\n',
  );
  await writeFile(
    join(root, "component/Cargo.toml"),
    '[package]\nname = "hibana-notice-fixture"\nversion = "0.1.0"\nedition = "2021"\n[lib]\ncrate-type = ["cdylib"]\n',
  );
  await writeFile(
    join(root, "component/src/lib.rs"),
    '#[no_mangle]\npub extern "C" fn fixture() -> u32 { 42 }\n',
  );
  await writeFile(
    join(root, "component/wit/world.wit"),
    "package hibana:notice-fixture;\nworld fixture {}\n",
  );
  await writeFile(
    join(folder, "unrelated/Cargo.toml"),
    '[package]\nname = "unrelated"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nitoa = "=1.0.18"\n',
  );
  await writeFile(join(folder, "unrelated/src/lib.rs"), "");
  await runCommand("cargo", ["generate-lockfile"], {
    cwd: folder,
    env: process.env,
    timeoutMs: 120000,
  });
  const lock = await readFile(join(folder, "Cargo.lock"), "utf8");
  const linkedRoot = join(folder, "selected");
  await symlink(root, linkedRoot, "dir");
  await buildComponent({
    root: linkedRoot,
    artifact: "hibana_notice_fixture.wasm",
    output: "fixture.wasm",
  });
  assert.ok((await readFile(join(root, "dist/fixture.wasm"))).length > 8);
  assert.equal(await readFile(join(folder, "Cargo.lock"), "utf8"), lock);
  assert.doesNotMatch(
    await readFile(join(root, "dist/NOTICE.txt"), "utf8"),
    /itoa/,
  );
  console.log(
    "PASS cold-cache extension build preserves the lockfile and excludes unrelated notices",
  );
} finally {
  if (previousCargoHome === undefined) delete process.env.CARGO_HOME;
  else process.env.CARGO_HOME = previousCargoHome;
  await rm(folder, { recursive: true, force: true });
}
