import { request } from "node:http";
import { setTimeout as delay } from "node:timers/promises";

// 100 Continue confirms that the handler acquired a slot and started reading
// the body. TCP connection completion alone does not confirm admission.
export async function incompleteRequest(
  port,
  sockets,
  { admissionTimeoutMs = 3000 } = {},
) {
  const deadline = performance.now() + admissionTimeoutMs;
  while (performance.now() < deadline) {
    const upload = request({
      hostname: "127.0.0.1",
      port,
      method: "POST",
      path: "/",
      agent: false,
      headers: {
        Expect: "100-continue",
        "Content-Length": "2",
        Connection: "close",
      },
    });
    upload.once("socket", (socket) => sockets.push(socket));
    let finishStatus, finishAdmission;
    const status = new Promise((resolve) => {
      finishStatus = resolve;
    });
    const admission = new Promise((resolve) => {
      finishAdmission = resolve;
    });
    upload.once("continue", () => finishAdmission(true));
    upload.once("response", (response) => {
      finishStatus(response.statusCode);
      finishAdmission(false);
      response.resume();
    });
    const closed = () => {
      finishStatus(0);
      finishAdmission(false);
    };
    upload.once("error", closed);
    upload.once("close", closed);
    const timer = setTimeout(
      () => upload.destroy(),
      Math.max(1, deadline - performance.now()),
    );
    upload.flushHeaders();
    const admitted = await admission;
    clearTimeout(timer);
    if (admitted) {
      upload.setTimeout(15000, () => upload.destroy());
      upload.write("x"); // Deliberately leave one byte unsent.
      return { socket: upload.socket, status };
    }
    upload.destroy();
    const code = await status;
    if (code !== 503)
      throw new Error(
        `Incomplete upload was not admitted (HTTP ${code || "closed/timeout"})`,
      );
    // A preceding complete response may still be releasing its execution slot.
    await delay(20);
  }
  throw new Error("Incomplete upload admission deadline exceeded");
}
