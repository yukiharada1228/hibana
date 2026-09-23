// Human output is deliberately small; --json preserves the complete API response.
export const text = (value) =>
  String(value ?? "—").replace(/[\u0000-\u001f\u007f-\u009f]/g, " ");
export const quote = (value) => `'${String(value).replaceAll("'", "'\\''")}'`;

export function versionLabel(version) {
  const automatic = /^0\.0\.0-dev\.\d+\.([a-f0-9]{8})$/.exec(version);
  return automatic ? `auto ${automatic[1]}` : text(version);
}

export function table(headers, rows, empty) {
  if (!rows.length) return console.log(empty);
  const values = [headers, ...rows].map((row) => row.map(text));
  const widths = headers.map((_, i) =>
    Math.max(...values.map((row) => row[i].length)),
  );
  for (const row of values)
    console.log(
      row
        .map((cell, i) => cell.padEnd(widths[i]))
        .join("  ")
        .trimEnd(),
    );
}

export function applications(rows, allTenants = false) {
  if (allTenants) {
    table(
      ["TENANT", "APP", "DEPLOYED"],
      rows.map((row) => [
        row.tenant_slug || row.tenant_id,
        row.name,
        row.active_version_id ? "Yes" : "No",
      ]),
      "No applications.",
    );
    return;
  }
  table(
    ["APP", "VERSION", "PUBLIC URL"],
    rows.map((row) => [
      row.name,
      row.active_version
        ? versionLabel(row.active_version)
        : row.active_version_id
          ? "Unknown"
          : "Not deployed",
      row.public_url ||
        (row.ingress_enabled ? "Not configured" : "Not published"),
    ]),
    "No applications. Run hibana deploy from your project directory.",
  );
}

export function secrets(result) {
  table(
    ["NAME", "VALUE", "UPDATED"],
    result.secrets.map((row) => [
      row.name,
      row.has_value ? "Stored" : "Missing",
      row.updated_at,
    ]),
    "No secrets. Register one with hibana secret put NAME < secret.txt",
  );
}

export function egressPolicy(policy) {
  if (!policy.allow_outbound.length)
    console.log("No destinations allowed. Outbound access is denied.");
  else
    table(
      ["ALLOWED DESTINATION"],
      policy.allow_outbound.map((host) => [host]),
      "",
    );
}
