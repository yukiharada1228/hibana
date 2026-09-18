// Browser contract fixtures only; excluded from the production image and bundle.
import { createServer } from "node:http";
let components, versions, egress, tokens, expired, unavailable, invocationCount;
function reset(empty = false) {
  components = empty
    ? []
    : [
        {
          component_id: "cmp_api",
          name: "hello-api",
          active_version_id: "ver_2",
          previous_active_version_id: "ver_1",
          ingress_enabled: true,
          created_at: "2026-09-15T00:00:00Z",
        },
        {
          component_id: "cmp_docs",
          name: "docs-preview",
          active_version_id: null,
          ingress_enabled: false,
          created_at: "2026-09-14T00:00:00Z",
        },
      ];
  versions = {
    cmp_api: [
      version("ver_2", "2.0.0"),
      version("ver_1", "1.0.0"),
      version("ver_old", "0.0.0-dev.1789484656916.d26b94ca"),
      { ...version("ver_busy", "0.8.0"), has_active_executions: true },
    ],
  };
  egress = {};
  tokens = new Map([["cli-fixture-token", ["read", "deploy", "admin"]]]);
  invocationCount = 12408;
  expired = false;
  unavailable = false;
}
function version(id, name) {
  return {
    version_id: id,
    version: name,
    status: "active",
    size_bytes: 12_400_000,
    wasm_sha256: "a".repeat(64),
    created_at: "2026-09-15T00:00:00Z",
  };
}
function componentView(c) {
  const active = (versions[c.component_id] || []).find(
    (v) => v.version_id === c.active_version_id,
  );
  return {
    ...c,
    active_version: active?.version || null,
    active_version_created_at: active?.created_at || null,
    public_url:
      c.ingress_enabled && active
        ? `https://${c.name}.team.apps.example.internal:8443/`
        : null,
  };
}
function deletionBlockedReason(component, v) {
  if (v.version_id === component.active_version_id) return "active_version";
  if (v.version_id === component.previous_active_version_id)
    return "rollback_target";
  return v.has_active_executions ? "active_executions" : null;
}
reset();
createServer(async (req, res) => {
  const url = new URL(req.url, "http://fixture.invalid");
  const send = (status, data) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(data === undefined ? undefined : JSON.stringify(data));
  };
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const raw = Buffer.concat(chunks).toString();
  const body =
    req.headers["content-type"]?.includes("application/json") && raw
      ? JSON.parse(raw)
      : {};
  if (url.pathname === "/healthz") return send(200, {});
  if (url.pathname === "/__test/reset") {
    reset(body.empty);
    return send(200, {});
  }
  if (url.pathname === "/__test/version-name") {
    const v = versions.cmp_api.find((item) => item.version_id === body.id);
    if (!v) return send(404, {});
    v.version = body.name;
    return send(200, {});
  }
  if (url.pathname === "/__test/activity") {
    invocationCount++;
    return send(200, {});
  }
  if (url.pathname === "/__test/expire") {
    expired = true;
    return send(200, {});
  }
  if (url.pathname === "/__test/unavailable") {
    unavailable = true;
    return send(200, {});
  }
  if (url.pathname === "/auth/login") {
    if (body.password !== "fixture-password") return send(401, {});
    const scopes =
      body.email === "reader@example.internal"
        ? ["read"]
        : body.email === "admin-only@example.internal"
          ? ["read", "admin"]
          : body.email === "deployer@example.internal"
            ? ["read", "deploy"]
            : ["read", "deploy", "admin"];
    const token = `browser-fixture-token-${tokens.size}`;
    tokens.set(token, scopes);
    return send(201, {
      token,
      expires_at: new Date(Date.now() + 3600000).toISOString(),
    });
  }
  const token = req.headers.authorization?.slice(7),
    scopes = tokens.get(token);
  if (!scopes || expired) return send(401, {});
  if (url.pathname === "/auth/session")
    return send(200, {
      tenant_id: "t_team",
      tenant_slug: "team",
      tenant_name: "Development",
      scopes,
      ingress_base_domain: "apps.example.internal",
    });
  if (url.pathname === "/auth/logout") {
    tokens.delete(token);
    return send(204);
  }
  if (unavailable) return send(503, {});
  if (url.pathname === "/components") {
    if (req.method === "POST") {
      const c = {
        component_id: `cmp_${body.name}`,
        name: body.name,
        active_version_id: null,
        ingress_enabled: false,
        created_at: new Date().toISOString(),
      };
      components.push(c);
      return send(201, c);
    }
    return send(200, components.map(componentView));
  }
  if (url.pathname === "/usage")
    return send(200, {
      from: url.searchParams.get("from"),
      to: url.searchParams.get("to"),
      totals: {
        invocation_count: invocationCount,
        succeeded_count: 12400,
        failed_count: 6,
        timeout_count: 2,
        wall_time_ms: 21578,
        output_bytes: 5420012,
      },
      by_component: [
        {
          component_id: "cmp_api",
          invocation_count: invocationCount,
          succeeded_count: 12400,
          failed_count: 6,
          timeout_count: 2,
          wall_time_ms: 21578,
          output_bytes: 5420012,
        },
      ],
    });
  const [, , id, action, versionName, versionId] = url.pathname
    .split("/")
    .map(decodeURIComponent);
  const component = components.find((c) => c.component_id === id);
  if (!component) return send(404, {});
  const selectedVersion = () =>
    (versions[id] || []).find((item) =>
      versionName === "by-id" && versionId
        ? item.version_id === versionId
        : item.version === versionName,
    );
  if (action === "egress") {
    if (req.method === "PATCH") {
      if (!scopes.includes("admin")) return send(403, {});
      const approved = new Set(egress[id] || []);
      for (const value of body.allow || []) approved.add(value);
      for (const value of body.deny || []) approved.delete(value);
      egress[id] = [...approved].sort();
    }
    return send(200, { allow_outbound: egress[id] ?? null });
  }
  if (req.method === "DELETE") {
    if (!scopes.includes("admin")) return send(403, {});
    if (action === "versions" && versionName) {
      const v = selectedVersion();
      if (!v) return send(404, {});
      if (deletionBlockedReason(component, v)) return send(409, {});
      versions[id] = versions[id].filter((item) => item !== v);
      return send(204);
    }
    if (action) return send(404, {});
    components = components.filter((c) => c !== component);
    return send(204);
  }
  if (action === "versions") {
    if (req.method === "GET" && versionName) {
      const v = selectedVersion();
      if (!v) return send(404, {});
      return send(200, {
        version_id: v.version_id,
        wasm_sha256: v.wasm_sha256,
        build_metadata:
          v.version === "2.0.0"
            ? {
                schema_version: 1,
                input: "javascript",
                roots: ["./database"],
                extensions: [
                  {
                    name: "./database",
                    version: null,
                    dependencies: ["@example/tls"],
                    permissions: [],
                  },
                  {
                    name: "@example/tls",
                    version: "1.2.3",
                    dependencies: ["@example/tcp"],
                    permissions: ["outbound-network"],
                  },
                  {
                    name: "@example/tcp",
                    version: "0.5.0",
                    dependencies: [],
                    permissions: ["outbound-network"],
                  },
                ],
              }
            : v.version === "1.0.0"
              ? {
                  schema_version: 1,
                  input: "javascript",
                  roots: [],
                  extensions: [],
                }
              : null,
        net_allow_outbound:
          egress[id] ??
          (v.version === "2.0.0" ? ["db.example.internal:5432"] : []),
      });
    }
    if (req.method === "POST") {
      const name = /name="version"\r\n\r\n([^\r]+)/.exec(raw)?.[1];
      const v = version(`ver_${id}_${name}`, name);
      versions[id] = [v, ...(versions[id] || [])];
      component.previous_active_version_id = component.active_version_id;
      component.active_version_id = v.version_id;
      component.ingress_enabled = true;
      return send(201, {
        ...v,
        public_url: componentView(component).public_url,
      });
    }
    return send(
      200,
      (versions[id] || []).map((v) => ({
        ...v,
        deletion_blocked_reason: deletionBlockedReason(component, v),
      })),
    );
  }
  if (action === "executions") {
    const offset = url.searchParams.has("before") ? 20 : 0;
    const items = Array.from({ length: offset ? 1 : 20 }, (_, i) => ({
      execution_id: `exe_${offset + i}`,
      version_id: component.active_version_id,
      status: "timeout",
      http_status: null,
      error: {
        code: "execution_timeout",
        message: "Execution deadline exceeded",
      },
      created_at: "2026-09-15T01:00:00Z",
      wall_time_ms: 1000,
    }));
    return send(200, {
      items,
      next_cursor: offset ? null : "2026-09-15T01:00:00Z,exe_19",
    });
  }
  if (action === "config")
    return send(200, {
      version_id: component.active_version_id,
      env: { GREETING: "Hello from Hibana", STAGE: "development" },
      secrets: [
        { name: "API_KEY", available: true },
        { name: "OLD_KEY", available: false },
      ],
      resource_limits: {
        max_memory_bytes: 134217728,
        max_wall_time_ms: 1000,
        max_execution_time_ms: 5000,
      },
      net_allow_outbound: egress[id] ?? ["db.example.internal:5432"],
    });
  if (action === "rollback") {
    if (!scopes.includes("deploy")) return send(403, {});
    const target = versions[id].find((v) => v.version === body.version);
    if (!target) return send(409, {});
    if (target.version_id !== component.active_version_id) {
      component.previous_active_version_id = component.active_version_id;
      component.active_version_id = target.version_id;
    }
    return send(200, { active_version_id: component.active_version_id });
  }
  send(404, {});
}).listen(4191, "127.0.0.1");
