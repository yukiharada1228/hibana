export type Session = {
  tenant_id: string;
  tenant_slug: string;
  tenant_name: string;
  scopes: string[];
};
export type Component = {
  public_url: string | null;
  active_version: string | null;
  active_version_created_at: string | null;
  component_id: string;
  name: string;
  active_version_id: string | null;
  ingress_enabled: boolean;
  created_at: string;
};
export type Version = {
  version_id: string;
  version: string;
  status: string;
  size_bytes: number;
  wasm_sha256: string;
  created_at: string;
  deletion_blocked_reason:
    | "active_version"
    | "rollback_target"
    | "active_executions"
    | null;
};
export type Extension = {
  name: string;
  version: string | null;
  dependencies: string[];
  permissions: string[];
};
export type VersionDetails = {
  version_id: string;
  wasm_sha256: string;
  build_metadata: {
    schema_version: 1;
    input: "javascript" | "component";
    roots: string[];
    extensions: Extension[];
  } | null;
  net_allow_outbound: string[];
};
export type Totals = {
  invocation_count: number;
  succeeded_count: number;
  failed_count: number;
  timeout_count: number;
  wall_time_ms: number;
  output_bytes: number;
  peak_memory_bytes_max: number;
};
export type Usage = {
  from: string;
  to: string;
  totals: Totals;
  by_component: (Totals & { component_id: string })[];
};

export type Settings = {
  version_id: string | null;
  env: Record<string, string>;
  secrets: { name: string; available: boolean }[];
  resource_limits: {
    max_memory_bytes: number;
    max_wall_time_ms: number;
    max_execution_time_ms: number;
  } | null;
  net_allow_outbound: string[];
};
export type Execution = {
  execution_id: string;
  version_id: string;
  status: string;
  error: unknown;
  created_at: string;
  wall_time_ms: number | null;
};
export type ExecutionsPage = { items: Execution[]; next_cursor: string | null };
export interface EgressPolicy {
  allow_outbound: string[] | null;
}
