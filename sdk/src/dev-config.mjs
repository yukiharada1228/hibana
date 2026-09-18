import { isIP } from "node:net";

// Local development permissions are never included in a deployment request.
export function devConfig(input) {
  if (input === undefined) return { allow_outbound: [] };
  if (!input || typeof input !== "object" || Array.isArray(input))
    throw new Error("dev must be an object with an allow_outbound list");
  if (Object.keys(input).some((key) => key !== "allow_outbound"))
    throw new Error("Unsupported dev field; use dev.allow_outbound");
  const entries =
    input.allow_outbound === undefined ? [] : input.allow_outbound;
  if (!Array.isArray(entries) || entries.length > 64)
    throw new Error(
      "dev.allow_outbound must list at most 64 HOST:PORT destinations",
    );
  const invalid = () =>
    new Error(
      "dev.allow_outbound requires HOST:PORT or [IPv6]:PORT, with ports 1..65535; URLs, credentials and wildcards are not allowed",
    );
  const destinations = entries.map((entry) => {
    if (typeof entry !== "string") throw invalid();
    const match = /^(?:\[([^\]]+)\]|([^:\s]+)):(\d+)$/.exec(entry.trim());
    if (!match) throw invalid();
    const [, ipv6, hostname, rawPort] = match;
    const port = Number(rawPort);
    if (!Number.isInteger(port) || port < 1 || port > 65535) throw invalid();
    if (ipv6) {
      if (isIP(ipv6) !== 6) throw invalid();
      return `[${ipv6.toLowerCase()}]:${port}`;
    }
    const host = hostname.toLowerCase().replace(/\.$/, "");
    if (
      host.length > 253 ||
      !host
        .split(".")
        .every((label) => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))
    )
      throw invalid();
    return `${host}:${port}`;
  });
  return { allow_outbound: [...new Set(destinations)] };
}
