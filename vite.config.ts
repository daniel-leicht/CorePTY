import { readFileSync } from "node:fs";
import { defineConfig } from "vite";

const version = JSON.parse(readFileSync("./package.json", "utf-8")).version;

// Vite config tuned for Tauri: fixed dev port, no clobbering the Tauri console,
// and don't watch the Rust side.
export default defineConfig({
  clearScreen: false,
  // Expose the package version to the app via the `__APP_VERSION__` global.
  define: {
    __APP_VERSION__: JSON.stringify(version),
  },
  server: {
    port: 1420,
    strictPort: true,
    host: false,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  // Produce assets Tauri can embed as a relative bundle.
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      output: {
        // Keep the application code separate from xterm/Tauri dependencies.
        // This avoids one oversized startup chunk and gives the webview a
        // stable vendor asset it can cache between application changes.
        manualChunks(id) {
          return id.includes("node_modules") ? "vendor" : undefined;
        },
      },
    },
  },
});
