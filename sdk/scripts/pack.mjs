// Only public operator manifests are copied into the distributable CLI.
// Local clusters, credentials, platform source and binaries stay in the checkout.
import { access, cp, mkdir, rm } from "node:fs/promises";
const license = new URL("../LICENSE", import.meta.url);
try { await cp(new URL("../../LICENSE", import.meta.url), license); }
catch (error) { if (error.code !== "ENOENT") throw error; await access(license); }
for (const name of ["base", "migration", "remote"]) {
  const destination = new URL(`../platform/manifests/${name}/`, import.meta.url);
  const source = new URL(`../../deploy/kubernetes/${name}/`, import.meta.url);
  try { await access(source); }
  catch (error) {
    if (error.code !== "ENOENT") throw error;
    // An already packed CLI can be packed again without the platform checkout.
    await access(new URL("kustomization.yaml", destination));
    continue;
  }
  await rm(destination, { recursive: true, force: true });
  await mkdir(destination, { recursive: true });
  await cp(source, destination, { recursive: true });
}
