import { spawn } from "node:child_process";

// Arguments are passed literally. hibana.json never implicitly invokes a shell.
export function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      stdio: "inherit",
      ...options,
      shell: false,
    });
    child.once("error", (error) =>
      reject(
        error.code === "ENOENT"
          ? new Error(
              `Command '${command}' was not found. Install it and make sure it is on PATH.`,
              { cause: error },
            )
          : error,
      ),
    );
    child.once("exit", (code, signal) =>
      code === 0
        ? resolve()
        : reject(new Error(`${command} failed (${signal || code})`)),
    );
  });
}

// Explicit stdin input for scripts, shared by passwords and secrets.
export async function readStdin(label, input = process.stdin) {
  if (input.isTTY)
    throw new Error(
      label === "Password"
        ? "Use --password-stdin < password.txt, or omit --password-stdin to enter the password interactively"
        : `Pipe the ${label.toLowerCase()} value through stdin; it is not accepted as a command argument`,
    );
  const chunks = [];
  let size = 0;
  for await (const chunk of input) {
    size += chunk.length;
    if (size > 4096) throw new Error(`${label} exceeds 4096 bytes`);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks)
    .toString("utf8")
    .replace(/\r?\n$/, "");
}
