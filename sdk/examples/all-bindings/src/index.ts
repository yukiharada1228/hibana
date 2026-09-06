// 全バインディングを 1 コンポーネントで行使するスモークアプリ。
// - /        : [vars] GREETING を読む
// - /kv      : KV put/get
// - /r2      : R2 put/get (MinIO 実体)
// - /d1      : D1 exec/prepare/first (隔離 SQLite)
// - /do      : Durable Object storage で counter を永続化
// - /q       : Queue producer で 1 通 enqueue
// - /qlast   : queue consumer が KV に書いた最終処理結果を読む
import { Hono } from "hono";
import { DurableObject } from "cloudflare:workers";

export class Counter extends DurableObject {
  async fetch(req: Request) {
    const ctx = (this as any).ctx;
    const u = new URL(req.url);
    // /arm  : 2 秒後に alarm() を予約  /fired : alarm() が動いたか(0/1)
    if (u.pathname === "/arm") {
      await ctx.storage.setAlarm(Date.now() + 2000);
      return new Response("armed");
    }
    if (u.pathname === "/fired") {
      return new Response(String(((await ctx.storage.get("fired")) as number) ?? 0));
    }
    // 既定: counter インクリメント（永続化の確認）。
    let n = ((await ctx.storage.get("n")) as number) ?? 0;
    n++;
    await ctx.storage.put("n", n);
    return new Response(String(n));
  }
  // DO alarm ハンドラ。fire されたら storage に印を残す（one-shot）。
  async alarm() {
    const ctx = (this as any).ctx;
    await ctx.storage.put("fired", 1);
  }
}

const app = new Hono<{ Bindings: any }>();

app.get("/", (c) => c.text("all-bindings " + c.env.GREETING));
app.get("/kv", async (c) => {
  await c.env.CACHE.put("k", "kv-ok");
  return c.text(await c.env.CACHE.get("k"));
});
app.get("/r2", async (c) => {
  await c.env.MEDIA.put("f", "r2-ok");
  const o = await c.env.MEDIA.get("f");
  return c.text(await o.text());
});
app.get("/d1", async (c) => {
  await c.env.DB.exec("CREATE TABLE IF NOT EXISTS t(v TEXT)");
  await c.env.DB.prepare("INSERT INTO t(v) VALUES(?)").bind("d1-ok").run();
  const r = await c.env.DB.prepare("SELECT v FROM t ORDER BY rowid DESC LIMIT 1").first();
  return c.text((r as any).v);
});
const counter = (c: any, path: string) =>
  c.env.COUNTER.get(c.env.COUNTER.idFromName("x")).fetch(new Request("https://do" + path));
app.get("/do", (c) => counter(c, "/"));
app.get("/alarm-arm", (c) => counter(c, "/arm"));
app.get("/alarm-fired", (c) => counter(c, "/fired"));
app.get("/q", async (c) => {
  await c.env.JOBS.send({ t: "q-ok" });
  return c.text("queued");
});
app.get("/qlast", async (c) => c.text((await c.env.CACHE.get("qlast")) ?? "(none)"));

export default {
  fetch: (r: Request, e: any, x: any) => app.fetch(r, e, x),
  async queue(b: any, e: any) {
    for (const m of b.messages) await e.CACHE.put("qlast", "processed:" + m.body.t);
  },
};
