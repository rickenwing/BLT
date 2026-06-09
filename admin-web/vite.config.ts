import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev server proxies /api to a locally-running blt-server admin listener.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": "http://127.0.0.1:7402",
    },
  },
  build: {
    outDir: "dist",
  },
});
