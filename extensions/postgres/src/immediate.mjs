// Only pg-pool's asynchronous argument-error callback uses this helper.
export function setImmediate(callback, ...args) {
  return setTimeout(() => callback(...args), 0);
}
