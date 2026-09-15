export function unsupported(feature) {
  throw Object.assign(
    new Error(`${feature} is not supported by @hibana/postgres`),
    {
      code: "ERR_NOT_SUPPORTED",
    },
  );
}
export function readFileSync() {
  unsupported("Host files; pass ssl.ca as PEM text instead");
}
export function lookup() {
  unsupported(
    "Native libpq; DNS for ordinary connections is handled by WASI sockets",
  );
}
export default function pgpass() {
  unsupported("pgpass; pass password explicitly");
}
