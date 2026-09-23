import { digest } from "@hibana/md5";
import { Buffer } from "node:buffer";

function hex(data) {
  return Buffer.from(digest(Buffer.from(data))).toString("hex");
}
// Legacy PostgreSQL authentication only. Selecting this function adds no SCRAM.
export async function md5(user, password, salt) {
  return "md5" + hex(Buffer.concat([Buffer.from(hex(password + user)), salt]));
}
