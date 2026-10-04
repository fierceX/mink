/* Shared by the browser reader and the repository checker. */
(function (root) {
  function slugifyHeading(text) {
    return text.trim().toLowerCase().replace(/[^\p{Letter}\p{Number}\s-]/gu, "").replace(/\s+/g, "-");
  }
  function assignHeadingIds(headings) {
    const used = new Set();
    for (const heading of headings) {
      const base = slugifyHeading(heading.textContent || "") || "section";
      let id = base, count = 0;
      while (used.has(id)) id = `${base}-${++count}`;
      used.add(id);
      heading.id = id;
    }
  }
  function resolveLink(source, href, manifest, siteBase) {
    if (!href || /^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(href)) return { kind: "external", href };
    const repoBase = new URL("https://repository.invalid/");
    const resolved = new URL(href, new URL(source, repoBase));
    const path = decodeURIComponent(resolved.pathname.slice(1));
    const doc = manifest.find(entry => entry.source === path);
    if (doc) return { kind: "doc", path: doc.path, anchor: decodeURIComponent(resolved.hash.slice(1)), href: `#docs/${encodeURI(doc.path)}${resolved.hash}` };
    if (path.startsWith("docs/assets/") || path === "docs/index.html" || path === "docs/manifest.json") {
      return { kind: "asset", href: new URL(path.slice(5) + resolved.search + resolved.hash, siteBase).href };
    }
    return { kind: "source", href: `https://github.com/fierceX/mink/blob/main/${resolved.pathname.slice(1)}${resolved.search}${resolved.hash}` };
  }
  const api = { slugifyHeading, assignHeadingIds, resolveLink };
  if (typeof module !== "undefined") module.exports = api;
  else root.MinkDocs = api;
})(typeof globalThis !== "undefined" ? globalThis : this);
