// A custom section travels with the artifact, including through signing and upload.
// This is a build declaration, not proof of the code's behavior or a permission grant.
export const BUILD_METADATA_SECTION = "hibana:build";
export const MAX_BUILD_METADATA_BYTES = 32 * 1024;
const HEADER = Buffer.from([0, 97, 115, 109, 13, 0, 1, 0]);

function leb(value) {
  const bytes = [];
  do {
    const next = value >>> 7;
    bytes.push((value & 127) | (next ? 128 : 0));
    value = next;
  } while (value);
  return Buffer.from(bytes);
}

// Only walk outer sections. Metadata inside a composed dependency does not
// describe the application. The server still validates the complete component.
function sections(bytes) {
  if (!bytes.subarray(0, 8).equals(HEADER))
    throw new Error("Build metadata requires a WebAssembly Component");
  let offset = 8;
  function length(end = bytes.length) {
    let value = 0;
    for (let i = 0; i < 5 && offset < end; i++) {
      const byte = bytes[offset++];
      if (i === 4 && byte > 15) break;
      value += (byte & 127) * 2 ** (i * 7);
      if (!(byte & 128)) return value;
    }
    throw new Error("Malformed WebAssembly section length");
  }
  const result = [];
  while (offset < bytes.length) {
    const start = offset;
    const id = bytes[offset++];
    const size = length();
    const end = offset + size;
    if (end > bytes.length) throw new Error("Truncated WebAssembly section");
    let metadata = false;
    if (id === 0) {
      const nameSize = length(end);
      const nameEnd = offset + nameSize;
      if (nameEnd > end)
        throw new Error("Truncated WebAssembly custom section");
      metadata =
        bytes.subarray(offset, nameEnd).toString("utf8") ===
        BUILD_METADATA_SECTION;
      offset = nameEnd;
    }
    result.push({ start, end, metadata, data: bytes.subarray(offset, end) });
    offset = end;
  }
  if (result.filter((section) => section.metadata).length > 1)
    throw new Error("Duplicate Hibana build metadata sections");
  return result;
}

export function readBuildMetadata(bytes) {
  const section = sections(bytes).find((section) => section.metadata);
  if (!section) return null;
  if (section.data.length > MAX_BUILD_METADATA_BYTES)
    throw new Error("Build metadata exceeds 32 KiB");
  try {
    return JSON.parse(section.data.toString("utf8"));
  } catch {
    throw new Error("Build metadata must be valid JSON");
  }
}

export function withBuildMetadata(
  bytes,
  metadata,
  { preserveExisting = false } = {},
) {
  const parts = sections(bytes);
  if (preserveExisting && parts.some((section) => section.metadata))
    return bytes;
  const json = Buffer.from(JSON.stringify(metadata));
  if (json.length > MAX_BUILD_METADATA_BYTES)
    throw new Error("Build metadata exceeds 32 KiB");
  const name = Buffer.from(BUILD_METADATA_SECTION);
  const payload = Buffer.concat([leb(name.length), name, json]);
  return Buffer.concat([
    HEADER,
    ...parts
      .filter((section) => !section.metadata)
      .map((section) => bytes.subarray(section.start, section.end)),
    Buffer.from([0]),
    leb(payload.length),
    payload,
  ]);
}
