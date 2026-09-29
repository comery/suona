import { defineConfig } from "vite";

// Tauri drives this dev server; the port must match `build.devUrl`.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
    watch: {
      // Rust sources are rebuilt by cargo, not vite.
      ignored: ["**/src-tauri/**", "**/.cargo-home/**", "**/.pnpm-store/**"],
    },
  },
  build: {
    // WKWebView on macOS 27 comfortably handles modern syntax.
    target: "safari15",
    emptyOutDir: true,
  },
});
