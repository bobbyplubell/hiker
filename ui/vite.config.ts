/// <reference types="vitest" />
import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "0.0.0.0",
    hmr: {
      host: "localhost",
      clientPort: 1420,
    },
    watch: {
      usePolling: false,
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2021",
  },
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
    globals: false,
  },
});
