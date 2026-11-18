import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";

// `npm run dev` talks to a daemon on this machine: `tm-daemon serve <project>`.
const daemon = process.env.TASKMASTER_DAEMON ?? "http://127.0.0.1:8737";

// dotLottie's player is wasm. Its package does not export the file, so it is named here and
// bundled with the app: nothing is fetched from a CDN.
const lottieWasm = fileURLToPath(new URL("./node_modules/@lottiefiles/dotlottie-web/dist/dotlottie-player.wasm", import.meta.url));

export default defineConfig({
  plugins: [react()],
  resolve: { alias: [{ find: /^dotlottie-player\.wasm/, replacement: lottieWasm }] },
  server: { proxy: { "/api": { target: daemon, changeOrigin: true } } },
  build: {
    outDir: "dist",
    assetsDir: "assets",
    sourcemap: false,
    // The files panel, the terminals and the Lottie player are imported lazily (src/lazy.tsx),
    // so each lands in its own chunk and loads when first shown.
    chunkSizeWarningLimit: 1200,
  },
});
