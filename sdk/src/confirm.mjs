import { createInterface } from "node:readline/promises";

export async function confirm(message, yes = false) {
  if (yes) return true;
  if (!process.stdin.isTTY) throw new Error("Use --yes to confirm in a non-interactive terminal");
  const input = createInterface({ input: process.stdin, output: process.stdout });
  try { return /^(y|yes)$/i.test((await input.question(`${message} [y/N] `)).trim()); }
  finally { input.close(); }
}
