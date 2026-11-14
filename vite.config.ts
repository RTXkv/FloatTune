import { defineConfig } from "vite";

// Tauri expects a fixed port and hates clearing the screen
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Rust 构建产物不归 vite 管（Windows 上会 EBUSY）
      ignored: ["**/src-tauri/target/**", "**/dist/**"],
    },
  },
  build: {
    target: "chrome110",
    outDir: "dist",
    minify: true,
  },
});
