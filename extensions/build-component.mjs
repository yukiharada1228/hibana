// Author-only build. Package consumers need neither Rust nor a C compiler.
import { spawnSync } from "node:child_process";
import {
  copyFile,
  mkdir,
  access,
  readdir,
  writeFile,
  rm,
} from "node:fs/promises";
import path from "node:path";

export async function buildComponent({ root, artifact, output }) {
  const env = { ...process.env };
  if (env.WASI_SDK_PATH) {
    env.CC_wasm32_wasip2 = path.join(env.WASI_SDK_PATH, "bin/clang");
    env.AR_wasm32_wasip2 = path.join(env.WASI_SDK_PATH, "bin/llvm-ar");
    await access(env.CC_wasm32_wasip2);
  }
  const result = spawnSync(
    "cargo",
    [
      "build",
      "--locked",
      "--release",
      "--target",
      "wasm32-wasip2",
      "--manifest-path",
      "component/Cargo.toml",
    ],
    { cwd: root, env, stdio: "inherit" },
  );
  if (result.error) throw result.error;
  if (result.status !== 0)
    throw new Error(
      "Component build failed; native dependencies may require WASI_SDK_PATH.",
    );
  await mkdir(path.join(root, "dist"), { recursive: true });
  await mkdir(path.join(root, "wit"), { recursive: true });
  const target = env.CARGO_TARGET_DIR
    ? path.resolve(root, env.CARGO_TARGET_DIR)
    : path.join(root, "component/target");
  await copyFile(
    path.join(target, "wasm32-wasip2/release", artifact),
    path.join(root, "dist", output),
  );
  await copyFile(
    path.join(root, "component/wit/world.wit"),
    path.join(root, "wit/world.wit"),
  );

  // Preserve notices for Rust code statically linked into the shipped Component.
  // Including build dependencies as well avoids omitting generated-code notices.
  const metadata = spawnSync(
    "cargo",
    [
      "metadata",
      "--locked",
      "--offline",
      "--filter-platform",
      "wasm32-wasip2",
      "--format-version",
      "1",
      "--manifest-path",
      "component/Cargo.toml",
    ],
    { cwd: root, env, encoding: "utf8", maxBuffer: 8 * 1024 * 1024 },
  );
  if (metadata.error) throw metadata.error;
  if (metadata.status !== 0) throw new Error(metadata.stderr);
  const licenseRoot = path.join(root, "dist/licenses");
  await rm(licenseRoot, { recursive: true, force: true });
  await mkdir(licenseRoot, { recursive: true });
  const notices = [
    "Rust dependencies (including build dependencies). Source and license information:",
    "",
  ];
  for (const pkg of JSON.parse(metadata.stdout)
    .packages.filter((pkg) => pkg.source)
    .sort((a, b) => a.name.localeCompare(b.name))) {
    const source = path.dirname(pkg.manifest_path);
    const destination = path.join(licenseRoot, `${pkg.name}-${pkg.version}`);
    notices.push(
      `${pkg.name} ${pkg.version}: ${pkg.license || "see license files"}`,
      `https://crates.io/api/v1/crates/${pkg.name}/${pkg.version}/download`,
      "",
    );
    async function collect(directory, relative = "") {
      for (const entry of await readdir(directory, { withFileTypes: true })) {
        const next = path.join(relative, entry.name);
        if (entry.isDirectory() && !["target", ".git"].includes(entry.name))
          await collect(path.join(directory, entry.name), next);
        else if (
          entry.isFile() &&
          /^(licen[sc]e|copying|notice)([._-]|$)/i.test(entry.name)
        ) {
          const output = path.join(destination, next);
          await mkdir(path.dirname(output), { recursive: true });
          await copyFile(path.join(directory, entry.name), output);
        }
      }
    }
    await collect(source);
  }
  await writeFile(path.join(root, "dist/NOTICE.txt"), notices.join("\n"));
}
