import type {
  Component,
  Session,
  Usage,
  Version,
  Settings,
  ExecutionsPage,
} from "./types";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

const messages: Record<number, string> = {
  401: "ログイン情報が無効か、有効期限が切れています。もう一度ログインしてください。",
  403: "この操作を行う権限がありません。",
  404: "対象が見つかりません。一覧を更新してください。",
  409: "現在の状態では操作できません。一覧を更新して確認してください。",
  429: "リクエストが集中しています。しばらく待ってから再度お試しください。",
  503: "基盤が処理を受け付けられません。しばらく待ってから状態を確認してください。",
};

/** One page session. Credentials never enter URLs, browser storage or cookies. */
export class Api {
  private token = "";
  private lifetime = new AbortController();
  onExpired = () => {};

  close() {
    this.token = "";
    this.lifetime.abort();
  }

  private async request<T>(
    path: string,
    method = "GET",
    body?: unknown,
  ): Promise<T> {
    let response: Response;
    try {
      response = await fetch(`/api${path}`, {
        method,
        credentials: "omit",
        redirect: "error",
        cache: "no-store",
        signal: AbortSignal.any([
          this.lifetime.signal,
          AbortSignal.timeout(120_000),
        ]),
        headers: {
          ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
          ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (error) {
      if (this.lifetime.signal.aborted) throw error;
      throw new Error(
        "基盤に接続できません。接続先とネットワークを確認してください。操作中だった場合は、再実行の前に状態を確認してください。",
      );
    }
    if (!response.ok) {
      if (response.status === 401 && this.token) {
        this.close();
        this.onExpired();
      }
      throw new ApiError(
        response.status,
        messages[response.status] ||
          `処理に失敗しました（HTTP ${response.status}）。`,
      );
    }
    return response.status === 204 ? (undefined as T) : response.json();
  }

  async login(
    tenant: string,
    email: string,
    password: string,
  ): Promise<{ session: Session; expires: string }> {
    const result = await this.request<{ token: string; expires_at: string }>(
      "/auth/login",
      "POST",
      {
        tenant_slug: tenant,
        email,
        password,
      },
    );
    this.token = result.token;
    try {
      const session = await this.request<Session>("/auth/session");
      if (!session.scopes.includes("read"))
        throw new Error("コンソールの利用には Read 権限が必要です。");
      return { session, expires: result.expires_at };
    } catch (error) {
      await this.request("/auth/logout", "POST").catch(() => {});
      this.close();
      throw error;
    }
  }
  async logout() {
    await this.request("/auth/logout", "POST");
    this.close();
  }
  components() {
    return this.request<Component[]>("/components");
  }
  versions(id: string) {
    return this.request<Version[]>(
      `/components/${encodeURIComponent(id)}/versions`,
    );
  }
  config(id: string) {
    return this.request<Settings>(
      `/components/${encodeURIComponent(id)}/config`,
    );
  }
  executions(id: string, errorsOnly: boolean, before?: string) {
    const query = new URLSearchParams({ errors_only: String(errorsOnly) });
    if (before) query.set("before", before);
    return this.request<ExecutionsPage>(
      `/components/${encodeURIComponent(id)}/executions?${query}`,
    );
  }
  rollback(id: string, version: string) {
    return this.request(
      `/components/${encodeURIComponent(id)}/rollback`,
      "POST",
      { version },
    );
  }
  deleteComponent(id: string) {
    return this.request(`/components/${encodeURIComponent(id)}`, "DELETE");
  }
  deleteVersion(id: string, version: string) {
    return this.request<void>(
      `/components/${encodeURIComponent(id)}/versions/${encodeURIComponent(version)}`,
      "DELETE",
    );
  }
  usage(from: string, to: string) {
    return this.request<Usage>(`/usage?${new URLSearchParams({ from, to })}`);
  }
}

/** Validate navigation; URL construction belongs exclusively to the platform. */
export function appUrl(component: Component): string | null {
  if (
    !component.public_url ||
    !component.ingress_enabled ||
    !component.active_version_id
  )
    return null;
  try {
    const url = new URL(component.public_url);
    if (
      url.username ||
      url.password ||
      url.search ||
      url.hash ||
      url.pathname !== "/"
    )
      return null;
    if (
      url.protocol !== "https:" &&
      !(url.protocol === "http:" && url.hostname.endsWith(".localhost"))
    )
      return null;
    return url.href;
  } catch {
    return null;
  }
}

export function publication(component: Component): string {
  if (!component.active_version_id) return "未配備";
  if (!component.ingress_enabled) return "非公開";
  return appUrl(component) ? "公開設定済み" : "公開 URL 未設定";
}

export const errorMessage = (error: unknown) =>
  error instanceof Error ? error.message : "処理に失敗しました。";
