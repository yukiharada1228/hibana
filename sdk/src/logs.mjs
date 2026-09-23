import { apiClient, findComponent } from "./api.mjs";
import { loadConfig } from "./config.mjs";
import { text, quote } from "./output.mjs";

export async function logs(args, options) {
  if (options.execution && (options.before || options["errors-only"]))
    throw new Error(
      "--execution cannot be combined with --before or --errors-only",
    );
  const api = await apiClient(options);
  let component;
  if (args[0] || !options.execution) {
    const name = args[0] || (await loadConfig(options.config)).name;
    component = await findComponent(api, name);
    if (!component) throw new Error("Application has not been deployed");
  }
  let result;
  if (options.execution) {
    const item = await api.request(
      `/executions/${encodeURIComponent(options.execution)}`,
    );
    if (component && item.component_id !== component.component_id)
      throw new Error("Execution does not belong to the selected application");
    result = { items: [item], next_cursor: null };
  } else {
    const query = new URLSearchParams();
    if (options.before) query.set("before", options.before);
    if (options["errors-only"]) query.set("errors_only", "true");
    result = await api.request(
      `/components/${encodeURIComponent(component.component_id)}/logs?${query}`,
    );
  }
  if (options.json) return console.log(JSON.stringify(result, null, 2));
  if (!result.items.length) console.log("No executions in the last 24 hours.");
  for (const item of result.items) {
    console.log(
      `${text(item.created_at)}  ${text(item.execution_id)}  ${text(item.version_id)}  ${text(item.status)}${item.http_status ? `  HTTP ${text(item.http_status)}` : ""}`,
    );
    if (!item.logs) {
      console.log(
        "  Logs unavailable (not completed, not captured, or expired).",
      );
      continue;
    }
    for (const stream of ["stdout", "stderr"]) {
      if (item.logs[stream]) {
        console.log(`  [${stream}]`);
        // Preserve lines, but never execute terminal escape/control sequences.
        for (const line of item.logs[stream].split("\n"))
          console.log(
            `  ${text(line).replace(/[\u202a-\u202e\u2066-\u2069]/g, " ")}`,
          );
      }
    }
    if (!item.logs.stdout && !item.logs.stderr)
      console.log("  No application output.");
    if (item.logs.truncated) console.log("  [Logs truncated at 16 KiB]");
  }
  if (result.next_cursor)
    console.log(`Next page: add --before ${quote(result.next_cursor)}`);
}
