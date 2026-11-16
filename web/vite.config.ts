import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `npm run dev` talks to a daemon on this machine: `tm-daemon serve <project>`.
const daemon = process.env.TASKMASTER_DAEMON ?? "http://127.0.0.1:8737";

export default defineConfig({
  plugins: [react()],
  server: { proxy: { "/api": { target: daemon, changeOrigin: true } } },
  build: { outDir: "dist", assetsDir: "assets", sourcemap: false },
});
