// These helpers are scoped to the prebundled driver, never aliased globally.
export function deprecate(fn, message) {
  let warned = false;
  return function (...args) {
    if (!warned) {
      warned = true;
      console.warn(message);
    }
    return fn.apply(this, args);
  };
}
