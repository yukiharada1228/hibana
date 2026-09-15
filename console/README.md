# Hibana Console

React + Vite の管理画面です。Kubernetes 上の Nginx が静的ファイルを配信し、同じオリジンの `/api/` を Control Plane へ転送します。配備済みアプリは Worker 上の Wasmtime で動きます。

導入、認証、検証手順は [コンソールの運用](../docs/console.md) を参照してください。

```sh
npm ci --prefix console --ignore-scripts
npm run build --prefix console
HIBANA_API_UPSTREAM=http://127.0.0.1:8080 npm run dev --prefix console
```

UI 開発時の `dev` はローカル開発用です。通常の利用者は管理者が導入したコンソールの URL をブラウザで開きます。

デザインは `shadcn-digital-agency-jp` のソース配布を利用しています。取り込み元とコミット、対象ファイルは [upstream.json](public/licenses/upstream.json) に記録しています。上流コンポーネントとテーマを変更するときは、ヘッダーと [ライセンス](public/licenses/THIRD_PARTY_LICENSES.md) を維持してください。Hibana 固有のレイアウトは `src/styles/app.css` です。
