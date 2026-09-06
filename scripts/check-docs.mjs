import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const docs = resolve(root, "docs");
const dist = resolve(docs, ".vitepress/dist");
const markdown = execFileSync(
  "git",
  ["ls-files", "--cached", "--others", "--exclude-standard", "-z", "*.md"],
  { cwd: root, encoding: "utf8" },
)
  .split("\0")
  .filter(Boolean);
const errors = new Set();
const vendored =
  /^(test\/bats\/|test\/test_helper\/bats-(assert|file|support)\/)/;
const external = /^(?:[a-z][a-z\d+.-]*:|\/\/)/i;

function targetFile(path, siteRoot) {
  for (const candidate of [path, `${path}.html`, resolve(path, "index.html")]) {
    if (existsSync(candidate) && statSync(candidate).isFile()) return candidate;
  }
  // Source links may name a Markdown page rather than the built HTML file.
  if (path.endsWith(".md")) {
    const html = path.replace(/\.md$/, ".html");
    if (existsSync(html)) return html;
  }
  if (path === siteRoot) return resolve(siteRoot, "index.html");
}

let checkedLinks = 0;
for (const name of markdown) {
  const source = readFileSync(resolve(root, name), "utf8");
  // Read the complete inventory, but leave upstream documentation link policy
  // to its owners: the vendored test helpers omit some upstream files.
  if (vendored.test(name)) continue;
  const text = source.replace(/^(`{3,}|~{3,})[^\n]*\n[\s\S]*?^\1\s*$/gm, "");
  const links = [
    ...text.matchAll(/\]\(<?([^\s)>]+)>?(?:\s+"[^"]*")?\)/g),
    ...text.matchAll(/^\[[^\]]+\]:\s*<?([^\s>]+)>?/gm),
  ];
  for (const match of links) {
    const href = match[1];
    if (external.test(href) || href.startsWith("#")) continue;
    checkedLinks++;
    const pathname = decodeURIComponent(href.split(/[?#]/)[0]);
    if (!pathname) continue;
    const file = pathname.startsWith("/")
      ? resolve(docs, `.${pathname}`)
      : resolve(root, dirname(name), pathname);
    const candidates = [
      file,
      `${file}.md`,
      resolve(file, "index.md"),
      resolve(docs, "public", `.${pathname}`),
    ];
    if (!candidates.some((candidate) => existsSync(candidate)))
      errors.add(`${name}: missing local target ${href}`);
  }
}

let builtPages = 0;
if (!existsSync(resolve(dist, "index.html"))) {
  errors.add(
    "Build the documentation first to check rendered page and fragment links: mise run docs:build",
  );
} else {
  const pages = readdirSync(dist, { recursive: true }).filter((name) =>
    name.endsWith(".html"),
  );
  const ids = new Map(
    pages.map((name) => {
      const file = resolve(dist, name);
      return [
        file,
        new Set(
          [...readFileSync(file, "utf8").matchAll(/\bid="([^"]+)"/g)].map(
            (match) => match[1],
          ),
        ),
      ];
    }),
  );
  builtPages = pages.length;
  for (const name of pages) {
    const file = resolve(dist, name);
    const html = readFileSync(file, "utf8");
    for (const [, href] of html.matchAll(/<a\b[^>]*\bhref="([^"]+)"/g)) {
      if (external.test(href)) continue;
      checkedLinks++;
      const [pathAndQuery, fragment] = href.split("#");
      const pathname = decodeURIComponent(pathAndQuery.split("?")[0]);
      const destination = pathname.startsWith("/")
        ? resolve(dist, `.${pathname}`)
        : resolve(dirname(file), pathname || name.split("/").at(-1));
      const target = targetFile(destination, dist);
      if (!target) errors.add(`${name}: missing built target ${href}`);
      else if (
        fragment &&
        extname(target) === ".html" &&
        !ids.get(target)?.has(decodeURIComponent(fragment))
      ) {
        errors.add(`${name}: missing fragment ${href}`);
      }
    }
  }
}

console.log(
  `Read ${markdown.length} Markdown files; checked ${checkedLinks} local links across source and ${builtPages} built pages.`,
);
if (errors.size) {
  for (const error of errors) console.error(error);
  process.exitCode = 1;
} else
  console.log("Local documentation links and rendered fragments are valid.");
