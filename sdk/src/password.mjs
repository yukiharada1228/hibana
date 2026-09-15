import { createInterface } from "node:readline";
import { Writable } from "node:stream";

/** Read a password without echoing input or recording readline history. */
export async function promptPassword({
  input = process.stdin,
  output = process.stderr,
} = {}) {
  if (!input.isTTY || !output.isTTY)
    throw new Error(
      "Use --password-stdin < password.txt when no interactive terminal is available",
    );
  const muted = new Writable({
    write(_chunk, _encoding, done) {
      done();
    },
  });
  const reader = createInterface({
    input,
    output: muted,
    terminal: true,
    historySize: 0,
  });
  try {
    output.write("Password: ");
    const password = await new Promise((resolve, reject) => {
      const cancel = () => reject(new Error("Login cancelled"));
      reader.once("SIGINT", cancel);
      reader.once("close", cancel);
      reader.question("", resolve);
    });
    if (!password) throw new Error("Password must not be empty");
    if (Buffer.byteLength(password, "utf8") > 4096)
      throw new Error("Password exceeds 4096 bytes");
    return password;
  } finally {
    reader.close();
    muted.end();
    output.write("\n");
  }
}
