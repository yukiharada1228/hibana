import { Hono } from 'hono';
import { bearerAuth } from 'hono/bearer-auth';
import { HTTPException } from 'hono/http-exception';

type Bindings = {
  API_TOKEN?: string;
  UPSTREAM_TOKEN?: string;
  UPSTREAM_URL?: string;
  RELEASE?: string;
};

const app = new Hono<{ Bindings: Bindings }>();

app.use('*', async (c, next) => {
  c.header('Cache-Control', 'no-store');
  c.header('X-Content-Type-Options', 'nosniff');
  c.header('X-App-Release', c.env.RELEASE || 'unknown');
  await next();
});

app.get('/health', c => c.json({ status: 'ok' }));
app.use('/items/*', async (c, next) => {
  const token = c.env.API_TOKEN;
  if (!token || !c.env.UPSTREAM_TOKEN) return c.json({ error: 'service_not_configured' }, 503);
  return bearerAuth({ token })(c, next);
});

app.get('/items/:sku', async c => {
  const sku = c.req.param('sku');
  if (!/^[A-Z0-9][A-Z0-9-]{0,31}$/.test(sku)) return c.json({ error: 'invalid_sku' }, 400);
  let origin: URL;
  try {
    origin = new URL(c.env.UPSTREAM_URL || '');
    if (!['https:', 'http:'].includes(origin.protocol) || origin.username || origin.password ||
        origin.pathname !== '/' || origin.search || origin.hash) throw new Error();
  } catch { return c.json({ error: 'service_not_configured' }, 503); }

  // Only the operator-configured origin is used. Client headers and URL parameters
  // cannot select a destination or replace the upstream credential.
  try {
    const response = await fetch(new URL(`/items/${sku}`, origin), {
      headers: { Authorization: `Bearer ${c.env.UPSTREAM_TOKEN}`, Accept: 'application/json' },
      redirect: 'manual',
    });
    if (response.status === 404) { await response.body?.cancel(); return c.json({ error: 'item_not_found' }, 404); }
    if (!response.ok) { await response.body?.cancel(); return c.json({ error: 'upstream_unavailable' }, 502); }
    const reader = response.body?.getReader();
    if (!reader) return c.json({ error: 'upstream_invalid_response' }, 502);
    let bytes = 0;
    const chunks: Uint8Array[] = [];
    try {
      for (;;) {
        const part = await reader.read();
        if (part.done) break;
        bytes += part.value.byteLength;
        if (bytes > 16384) { await reader.cancel(); return c.json({ error: 'upstream_invalid_response' }, 502); }
        chunks.push(part.value);
      }
    } finally { reader.releaseLock(); }
    const buffer = new Uint8Array(bytes);
    let offset = 0;
    for (const chunk of chunks) { buffer.set(chunk, offset); offset += chunk.byteLength; }
    const item = JSON.parse(new TextDecoder().decode(buffer));
    if (!item || item.sku !== sku || typeof item.name !== 'string' || item.name.length > 200 ||
        !Number.isSafeInteger(item.available) || item.available < 0) {
      return c.json({ error: 'upstream_invalid_response' }, 502);
    }
    // Return the public contract, never upstream headers, internal fields or errors.
    return c.json({ sku: item.sku, name: item.name, available: item.available });
  } catch { return c.json({ error: 'upstream_unavailable' }, 502); }
});

app.onError((error, c) => error instanceof HTTPException ? error.getResponse() : c.json({ error: 'internal_error' }, 500));
export default app;
