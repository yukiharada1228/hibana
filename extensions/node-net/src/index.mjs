import {
  Socket,
  normalizeConnect,
  unsupported,
  baseOptions,
  isIP,
  isIPv4,
  isIPv6,
} from "./socket.mjs";
export { Socket, isIP, isIPv4, isIPv6 };
export function connect(...args) {
  const { options, callback } = normalizeConnect(args);
  const streamOptions = Object.fromEntries(
    baseOptions
      .filter((key) => key in options)
      .map((key) => [key, options[key]]),
  );
  const socket = new Socket(streamOptions);
  return callback ? socket.connect(options, callback) : socket.connect(options);
}
export const createConnection = connect;
export function createServer() {
  unsupported("TCP servers");
}
export class Server {
  constructor() {
    unsupported("TCP servers");
  }
}
export default {
  Socket,
  connect,
  createConnection,
  createServer,
  Server,
  isIP,
  isIPv4,
  isIPv6,
};
