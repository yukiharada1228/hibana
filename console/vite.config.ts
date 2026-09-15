import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwind from "@tailwindcss/vite";
import { fileURLToPath } from "node:url";

export default defineConfig({
  plugins: [react(), tailwind()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: {
    port: 5173,
    strictPort: true,
    // Used only to develop the console itself. Production uses nginx on Kubernetes.
    proxy: {
      "/api/": {
        target: process.env.HIBANA_API_UPSTREAM || "http://127.0.0.1:8080",
        rewrite: (path) => path.slice(4),
      },
    },
  },
});
