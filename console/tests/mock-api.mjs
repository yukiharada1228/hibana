// Browser contract fixtures only; excluded from the production image and bundle.
import { createServer } from "node:http";
let components, versions, tokens, expired, unavailable;
function reset(empty = false) {
  components = empty
    ? []
    : [
        {
          component_id: "cmp_api",
          name: "hello-api",
          active_version_id: "ver_2",
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
    cmp_api: [version("ver_2", "2.0.0"), version("ver_1", "1.0.0")],
  };
  tokens = new Map([["cli-fixture-token", ["read", "deploy", "admin"]]]);
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
    return send(200, components);
  }
  if (url.pathname === "/usage")
    return send(200, {
      from: url.searchParams.get("from"),
      to: url.searchParams.get("to"),
      totals: {
        invocation_count: 12408,
        succeeded_count: 12400,
        failed_count: 6,
        timeout_count: 2,
        wall_time_ms: 21578,
        output_bytes: 5420012,
      },
      by_component: [
        {
          component_id: "cmp_api",
          invocation_count: 12408,
          succeeded_count: 12400,
          failed_count: 6,
          timeout_count: 2,
          wall_time_ms: 21578,
          output_bytes: 5420012,
        },
      ],
    });
  const [, , id, action] = url.pathname.split("/");
  const component = components.find((c) => c.component_id === id);
  if (!component) return send(404, {});
  if (req.method === "DELETE") {
    components = components.filter((c) => c !== component);
    return send(204);
  }
  if (action === "versions") {
    if (req.method === "POST") {
      const name = /name="version"\r\n\r\n([^\r]+)/.exec(raw)?.[1];
      const v = version(`ver_${id}_${name}`, name);
      versions[id] = [v, ...(versions[id] || [])];
      component.active_version_id = v.version_id;
      component.ingress_enabled = true;
      return send(201, v);
    }
    return send(200, versions[id] || []);
  }
  if (action === "config")
    return send(200, {
      env: { GREETING: "Hello from Hibana", STAGE: "development" },
    });
  if (action === "rollback") {
    component.active_version_id = versions[id].find(
      (v) => v.version === body.version,
    ).version_id;
    return send(200, { active_version_id: component.active_version_id });
  }
  send(404, {});
}).listen(4191, "127.0.0.1");
