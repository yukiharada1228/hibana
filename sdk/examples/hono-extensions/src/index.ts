import { Hono } from 'hono';
import { createHash } from 'node:crypto';

const app = new Hono();
app.get('/', c => {
  const text = c.req.query('text') ?? 'abc';
  return c.json({
    sha256: createHash('sha256').update(text).digest('hex'),
    base64: Buffer.from(text).toString('base64'),
  });
});
export default app;
