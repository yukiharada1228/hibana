// Let the parent start competing publications after both installs captured the
// same previous lock. This exercises independent CLI processes, not a JS mutex.
import { once } from "node:events";
import { installExtensionPackages } from "../../src/extension-packages.mjs";

try {
  const prepared = await installExtensionPackages(JSON.parse(process.argv[2]));
  const start = once(process, "message");
  process.send("ready");
  await start;
  process.send("committing");
  await prepared.commitLock();
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
} finally {
  process.disconnect();
}
