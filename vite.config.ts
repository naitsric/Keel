import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  // IdeDB uses 1420, so both apps can run side by side.
  server: { port: 1421, strictPort: true, watch: { ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"] } },
  // WKWebView on macOS 13+ is Safari 16 class.
  build: { target: "safari16", chunkSizeWarningLimit: 2000 },
  test: { environment: "happy-dom" },
} as Parameters<typeof defineConfig>[0]);
