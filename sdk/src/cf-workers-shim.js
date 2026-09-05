// `cloudflare:workers` の最小 shim（esbuild alias で解決）。
// ユーザーの Durable Object クラスは `import { DurableObject } from "cloudflare:workers"` して
// これを継承する。基底は ctx/env を保持するだけ（Workers 同様 this.ctx / this.env でアクセス可能）。
export class DurableObject {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.state = ctx; // 旧 API 互換（this.state.storage）
    this.env = env;
  }
}

// WorkerEntrypoint 等の最小スタブ（存在すればビルドが通るように）。
export class WorkerEntrypoint {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
}
