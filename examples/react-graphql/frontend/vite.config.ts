import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Two modes, one config:
//
// - `npm run dev` serves `index.html` from a Vite dev server with hot module
//   replacement and proxies `/graphql` to the Autumn backend on :3000, so
//   editing a component never restarts the Rust server.
// - `npm run build` compiles `src/main.tsx` into `../static/app/app.js` and
//   `app.css` with FIXED file names (no content hash). Autumn already serves
//   `static/` at `/static/`, and its `asset_url()` helper fingerprints assets
//   in release builds, so the bundle needs no hash of its own — and the Maud
//   shell in `src/main.rs` can reference the two files by name.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/graphql": "http://127.0.0.1:3000",
    },
  },
  build: {
    outDir: "../static/app",
    emptyOutDir: true,
    // No source maps in the committed bundle: it is a build product checked
    // in so `cargo run -p react-graphql` works without a Node toolchain.
    sourcemap: false,
    rollupOptions: {
      input: "src/main.tsx",
      output: {
        entryFileNames: "app.js",
        chunkFileNames: "chunk-[name].js",
        assetFileNames: "app.[ext]",
      },
    },
  },
});
