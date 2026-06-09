import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri dev server settings per the Tauri v2 Vite guide.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    outDir: "dist",
  },
});
