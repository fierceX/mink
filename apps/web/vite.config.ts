// `vitest/config` re-exports vite's defineConfig with the `test` block typed,
// so one file owns both the build and the unit lane.
import { defineConfig, type Plugin } from "vitest/config";
import react from "@vitejs/plugin-react";

/** Strip legacy `.woff` / `.ttf` sources from KaTeX `@font-face` rules so Vite emits `.woff2` only. */
function katexWoff2OnlyPlugin(): Plugin {
  return {
    name: "mink-katex-woff2-only",
    enforce: "pre",
    transform(code, id) {
      if (!id.includes("katex") || !id.endsWith(".css")) return null;
      const rewritten = code.replace(/src:([^;]+);/g, (full, decl: string) => {
        const woff2 = decl
          .split(",")
          .map((part) => part.trim())
          .filter((part) => part.includes(".woff2"));
        return woff2.length > 0 ? `src:${woff2.join(",")};` : full;
      });
      return rewritten === code ? null : { code: rewritten, map: null };
    },
  };
}

/**
 * Mink web workbench.
 *
 * `base: "./"` keeps the bundle loadable from the embedded server's root and
 * from a sub-path; `/api` is proxied to a locally running `mink-server` so the
 * dev server reads real sessions instead of fixtures.
 */
export default defineConfig({
  base: "./",
  plugins: [react(), katexWoff2OnlyPlugin()],
  server: {
    host: "127.0.0.1",
    proxy: { "/api": "http://127.0.0.1:18821" },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rolldownOptions: {
      output: {
        manualChunks(id) {
          if (id.includes("node_modules/react/") || id.includes("node_modules/react-dom/") || id.includes("node_modules/scheduler/")) {
            return "react-vendor";
          }
          if (id.includes("node_modules/katex/")) {
            return "katex";
          }
          if (id.includes("node_modules/@shikijs/core/") || id.includes("node_modules/@shikijs/engine-javascript/") || id.includes("node_modules/shiki/")) {
            return "shiki-core";
          }
          return undefined;
        },
      },
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["src/test/setup.ts"],
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
  },
});
