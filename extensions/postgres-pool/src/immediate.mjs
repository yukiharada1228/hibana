// pg-pool reports asynchronous argument errors without retaining a Node process.
export function setImmediate(callback, ...args) {
  return setTimeout(() => callback(...args), 0);
}
