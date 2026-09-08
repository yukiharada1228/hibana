// Deterministic inventory service for the isolated acceptance network only.
import http from 'node:http';
const token = process.env.UPSTREAM_TOKEN;
if (!token) throw new Error('UPSTREAM_TOKEN is required');
let requests = 0;
http.createServer(async (req, res) => {
  res.setHeader('Content-Type', 'application/json');
  if (req.url === '/health') return res.end(JSON.stringify({status:'ok',requests}));
  requests++;
  if (req.headers.authorization !== `Bearer ${token}`) { res.statusCode=401; return res.end('{"error":"unauthorized"}'); }
  const sku = new URL(req.url, 'http://fixture').pathname.split('/').at(-1);
  if (sku === 'REDIRECT') { res.writeHead(302, {location:'http://169.254.169.254/'}); return res.end(); }
  if (sku === 'ERROR') { res.statusCode=500; return res.end(JSON.stringify({private:token})); }
  if (sku === 'LARGE') return res.end(JSON.stringify({name:'x'.repeat(20000)}));
  if (sku === 'INVALID') return res.end('{"sku":"INVALID","name":"Invalid","available":-1}');
  if (sku === 'MISSING') { res.statusCode=404; return res.end('{}'); }
  if (sku !== 'PEN-001') { res.statusCode=404; return res.end('{}'); }
  res.end(JSON.stringify({sku, name:'Hibana pen', available:42, internal_token:token}));
}).listen(8080, '0.0.0.0');
