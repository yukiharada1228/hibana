import { setTimeout as sleep } from "node:timers/promises";
import { apiClient, ApiError, findComponent } from "./api.mjs";
import { readConfigFile } from "./config.mjs";
import { isApplicationName } from "./application-name.mjs";
import { text } from "./output.mjs";

const safe = (value) =>
  text(value).replace(/[\u202a-\u202e\u2066-\u2069]/g, " ");
const outcome = (item) => (item.status === "succeeded" ? "ok" : "error");

const dateFormatter = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  timeZoneName: "short",
  hour12: false,
});

function formatExecution(item, { format, verbose = false }) {
  if (format === "json")
    return JSON.stringify({ ...item, outcome: outcome(item) });
  const time = dateFormatter.format(new Date(item.created_at));
  const label =
    item.status === "succeeded"
      ? "Ok"
      : item.status === "timeout"
        ? "Timeout"
        : "Error";
  const lines = [
    `${item.http_status ? `HTTP ${safe(item.http_status)}` : "Execution"} - ${label} (${time}${item.wall_time_ms == null ? "" : `, ${safe(item.wall_time_ms)} ms`})`,
  ];
  if (verbose)
    lines.push(`  ${safe(item.execution_id)}  ${safe(item.version_id)}`);
  for (const stream of ["stdout", "stderr"]) {
    const output = item.logs?.[stream];
    if (output) {
      // Strip only the final line delimiter, preserving meaningful blank lines.
      for (const line of output.replace(/\r?\n$/, "").split("\n"))
        lines.push(`  [${stream}] ${safe(line)}`);
    }
  }
  if (item.error != null)
    lines.push(
      `  [exception] ${safe(item.error.message ?? JSON.stringify(item.error))}`,
    );
  if (item.logs == null) lines.push("  [Application logs unavailable]");
  if (item.logs?.truncated) lines.push("  [Logs truncated at 16 KiB]");
  return lines.join("\n");
}

function validate(options) {
  if (options.format && !["pretty", "json"].includes(options.format))
    throw new Error("--format must be pretty or json");
  if (options.status && !["ok", "error", "canceled"].includes(options.status))
    throw new Error("--status must be ok, error or canceled");
  if (
    options.search !== undefined &&
    (!options.search || Buffer.byteLength(options.search) > 1024)
  )
    throw new Error("--search must contain 1..1024 UTF-8 bytes");
  if (
    options["version-id"] !== undefined &&
    (!options["version-id"] || Buffer.byteLength(options["version-id"]) > 128)
  )
    throw new Error("--version-id must contain 1..128 UTF-8 bytes");
}

function validPage(page) {
  return (
    page &&
    typeof page.cursor === "string" &&
    /^\d{1,20}-\d{1,20}$/.test(page.cursor) &&
    typeof page.has_more === "boolean" &&
    typeof page.lagged === "boolean" &&
    Array.isArray(page.items) &&
    page.items.length <= 100 &&
    page.items.every(
      (item) =>
        item &&
        typeof item.execution_id === "string" &&
        ["succeeded", "failed", "timeout"].includes(item.status) &&
        Number.isFinite(Date.parse(item.created_at)) &&
        (item.logs == null ||
          (typeof item.logs.stdout === "string" &&
            typeof item.logs.stderr === "string")),
    )
  );
}

export async function tail(args, options) {
  validate(options);
  const controller = new AbortController();
  const { signal } = controller;
  let interrupted = false,
    writing = false;
  const stop = () => {
    interrupted = true;
    controller.abort();
  };
  let outputError;
  const failedOutput = (error) => {
    outputError = error;
    controller.abort();
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
  process.stdout.on("error", failedOutput);
  const write = (value) =>
    new Promise((resolve, reject) => {
      signal.throwIfAborted();
      const aborted = () => reject(signal.reason);
      signal.addEventListener("abort", aborted, { once: true });
      writing = true;
      process.stdout.write(`${value}\n`, (error) => {
        writing = false;
        signal.removeEventListener("abort", aborted);
        error ? reject(error) : resolve();
      });
    });
  try {
    const name = args[0] || (await readConfigFile(options.config)).name;
    if (!isApplicationName(name))
      throw new Error("Application name must be a lowercase DNS label");
    const api = await apiClient(options);
    const component = await findComponent(api, name, { signal });
    if (!component) throw new Error("Application has not been deployed");
    const query = new URLSearchParams();
    for (const [flag, param] of [
      ["status", "status"],
      ["search", "search"],
      ["version-id", "version_id"],
    ])
      if (options[flag]) query.set(param, options[flag]);
    const format = options.format ?? (process.stdout.isTTY ? "pretty" : "json");
    // Bounded duplicate suppression for ambiguous network delivery. Cursor advances
    // only after writing the whole page, so slow stdout naturally applies backpressure.
    const seen = new Set();
    let cursor,
      failures = 0;
    while (!signal.aborted) {
      if (cursor) query.set("cursor", cursor);
      let page;
      try {
        page = await api.request(
          `/components/${encodeURIComponent(component.component_id)}/tail?${query}`,
          { signal },
        );
      } catch (error) {
        if (signal.aborted) break;
        if (error instanceof ApiError && error.status === 404) {
          error.hint =
            "Live tail is unavailable. Check that the application still exists and the Control Plane supports hibana tail.";
          throw error;
        }
        const retry =
          error instanceof ApiError
            ? error.status === 429 || error.status >= 500
            : error instanceof TypeError ||
              error.name === "TimeoutError" ||
              error.retryableRead === true;
        if (!retry) throw error;
        if (failures++ === 0)
          console.error(
            "Tail connection lost; reconnecting. Live events may be missed during the interruption.",
          );
        await sleep(
          Math.min(1000 * 2 ** Math.min(failures - 1, 4), 10000),
          undefined,
          { signal },
        );
        continue;
      }
      if (!validPage(page))
        throw new Error(
          "Invalid live tail response. Update the CLI and Control Plane to compatible versions.",
        );
      if (!cursor)
        console.error(
          `Connected to ${safe(name)}. Waiting for completed executions; press Ctrl+C to stop.`,
        );
      else if (failures) console.error("Tail reconnected.");
      failures = 0;
      if (page.lagged)
        console.error(
          "Warning: live events were dropped because the tail buffer expired, reset or filled up. View stored logs in the Console's execution history.",
        );
      for (const item of page.items) {
        if (signal.aborted) break;
        if (seen.has(item.execution_id)) continue;
        await write(
          formatExecution(item, { format, verbose: options.verbose }),
        );
        seen.add(item.execution_id);
        if (seen.size > 1000) seen.delete(seen.values().next().value);
      }
      cursor = page.cursor;
      if (!page.has_more) await sleep(1000, undefined, { signal });
    }
  } catch (error) {
    if (!signal.aborted && error.code !== "EPIPE") throw error;
  } finally {
    process.off("SIGINT", stop);
    process.off("SIGTERM", stop);
    process.stdout.off("error", failedOutput);
    // Node's stdout cannot be destroyed like an ordinary stream. After explicit
    // interruption, discard a blocked write rather than waiting for the reader.
    if (interrupted && writing) process.exit(0);
  }
  if (!interrupted && outputError && outputError.code !== "EPIPE")
    throw outputError;
}
