// `vitest/config` re-exports vite's defineConfig with the `test` block typed,
// so one file owns both the build and the unit lane.
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

/**
 * Mink web workbench.
 *
 * `base: "./"` keeps the bundle loadable from the embedded server's root and
 * from a sub-path; `/api` is proxied to a locally running `mink-server` so the
 * dev server reads real sessions instead of fixtures.
 */
export default defineConfig({
  base: "./",
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    proxy: { "/api": "http://127.0.0.1:18821" },
  },
  build: { outDir: "dist", emptyOutDir: true },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["src/test/setup.ts"],
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
  },
});
