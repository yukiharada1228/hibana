import { isAbsolute, relative } from "node:path";

// Resolve symlinks with realpath before checking filesystem boundaries.
export function isInside(root, path) {
  const part = relative(root, path);
  return (
    part !== ".." &&
    !part.startsWith("../") &&
    !part.startsWith("..\\") &&
    !isAbsolute(part)
  );
}
