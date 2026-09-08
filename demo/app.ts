import { Hono } from 'hono'

const app = new Hono()
let count = 0

app.get('/', (c) => c.json({
  message: 'Hello, Wasm!',
  count: ++count,
}))

export default app
