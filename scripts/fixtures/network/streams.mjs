// Exercise only selective entries: importing node:stream would hide missing
// initialization and reintroduce the operators whose absence we are testing.
import { Readable } from "@hibana/node-stream/readable";
import { Writable } from "@hibana/node-stream/writable";
import { Duplex } from "@hibana/node-stream/duplex";
import { Transform } from "@hibana/node-stream/transform";
import { PassThrough } from "@hibana/node-stream/passthrough";
import { pipeline } from "@hibana/node-stream/pipeline";
import { compose } from "@hibana/node-stream/compose";
import { finished } from "@hibana/node-stream/finished";

const runPipeline = (...streams) =>
  new Promise((resolve, reject) =>
    pipeline(...streams, (error) => (error ? reject(error) : resolve())),
  );

export async function verify() {
  let text = "";
  const sink = new Writable({
    highWaterMark: 1,
    write(chunk, encoding, done) {
      // Force upstream writers to wait for drain.
      setTimeout(() => {
        text += chunk;
        done();
      }, 1);
    },
  });
  let notifications = 0;
  let cleanup;
  const completed = new Promise((resolve, reject) => {
    cleanup = finished(sink, (error) => {
      notifications++;
      error ? reject(error) : resolve();
    });
  });
  const pass = new PassThrough({
    readableObjectMode: true,
    writableObjectMode: false,
    readableHighWaterMark: 3,
    writableHighWaterMark: 1,
  });
  const upper = new Transform({
    transform(chunk, encoding, done) {
      done(null, chunk.toString().toUpperCase());
    },
  });
  await Promise.all([
    runPipeline(
      Readable.from(["hello", " stream", " 日本語 🔥"]),
      compose(pass, upper),
      sink,
    ),
    completed,
  ]);
  cleanup();

  const failure = Object.assign(new Error("Intentional stream failure"), {
    code: "ERR_STREAM_FIXTURE",
  });
  const source = Readable.from(["input"]);
  const broken = new Transform({
    transform(chunk, encoding, done) {
      done(failure);
    },
  });
  const discarded = new Writable({
    write(chunk, encoding, done) {
      done();
    },
  });
  let error;
  try {
    await runPipeline(source, broken, discarded);
  } catch (cause) {
    error = cause.code;
  }
  return {
    text,
    notifications,
    inherited:
      pass instanceof Transform &&
      pass instanceof Duplex &&
      pass instanceof Readable &&
      pass instanceof Writable,
    separateModes: [
      pass.readableObjectMode,
      pass.writableObjectMode,
      pass.readableHighWaterMark,
      pass.writableHighWaterMark,
    ],
    operators: typeof Readable.prototype.map,
    finished: sink.writableFinished,
    error,
    released: [source, broken, discarded].every((stream) => stream.destroyed),
  };
}

export default {
  async fetch() {
    return Response.json(await verify());
  },
};
