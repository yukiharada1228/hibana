import { spawn } from "node:child_process";

// Arguments are passed literally. hibana.json never implicitly invokes a shell.
export function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", ...options, shell: false });
    child.once("error", error => reject(error.code === "ENOENT"
      ? new Error(`Command '${command}' was not found. Install it and make sure it is on PATH.`, { cause: error })
      : error));
    child.once("exit", (code, signal) => code === 0 ? resolve() : reject(new Error(`${command} failed (${signal || code})`)));
  });
}
