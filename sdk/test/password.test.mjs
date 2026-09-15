import test from "node:test";
import assert from "node:assert/strict";
import { PassThrough, Writable } from "node:stream";
import { promptPassword } from "../src/password.mjs";

function terminal() {
  const input = new PassThrough();
  input.isTTY = true;
  input.setRawMode = (value) => {
    input.isRaw = value;
  };
  let written = "";
  const output = new Writable({
    write(chunk, _encoding, done) {
      written += chunk.toString();
      done();
    },
  });
  output.isTTY = true;
  return { input, output, text: () => written };
}

test("password prompt supports editing and Unicode without echoing the password", async () => {
  const tty = terminal();
  const answer = promptPassword(tty);
  assert.equal(tty.input.isRaw, true);
  tty.input.write("hidden-あx\x7f🔥\r");
  assert.equal(await answer, "hidden-あ🔥");
  assert.equal(tty.text(), "Password: \n");
  assert.equal(tty.input.isRaw, false);
});

test("Ctrl+C and EOF cancel without exposing input or leaving raw mode enabled", async () => {
  for (const end of [(input) => input.write("\x03"), (input) => input.end()]) {
    const tty = terminal();
    const answer = promptPassword(tty);
    tty.input.write("discarded-password");
    end(tty.input);
    await assert.rejects(answer, /Login cancelled/);
    assert.equal(tty.text(), "Password: \n");
    assert.equal(tty.input.isRaw, false);
  }
});

test("empty and oversized passwords are rejected, with terminal mode restored", async () => {
  for (const [password, error] of [
    ["", /must not be empty/],
    ["あ".repeat(1366), /exceeds 4096 bytes/],
  ]) {
    const tty = terminal();
    const answer = promptPassword(tty);
    tty.input.write(password + "\r");
    await assert.rejects(answer, error);
    assert.equal(tty.text(), "Password: \n");
    assert.equal(tty.input.isRaw, false);
  }
});

test("non-interactive input gives an actionable stdin example", async () => {
  const tty = terminal();
  tty.input.isTTY = false;
  await assert.rejects(promptPassword(tty), /--password-stdin < password.txt/);
  assert.equal(tty.text(), "");
});
