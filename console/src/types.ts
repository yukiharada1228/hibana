export type Session = {
  tenant_id: string;
  tenant_slug: string;
  tenant_name: string;
  scopes: string[];
  ingress_base_domain: string | null;
};
export type Component = {
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
