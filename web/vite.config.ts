import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: {
    // `npm run dev` serves the UI; the API still comes from the Rust server on
    // its own port, so the token and the same-origin fetches in api.ts work
    // unchanged between development and a built bundle.
    proxy: {
      "/api": {
        target: "http://127.0.0.1:47810",
        changeOrigin: false,
      },
    },
  },
  build: {
    // Served from disk by sunburst-web's static handler, not by a CDN.
    outDir: "dist",
    emptyOutDir: true,
  },
});
