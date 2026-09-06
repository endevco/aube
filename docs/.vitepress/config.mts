import { socialCard, writeSocialCard } from "./social-images.mjs";
import { defineConfig } from "vitepress";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import spec from "../cli/commands.json";

interface Cmd {
  name: string;
  full_cmd: string[];
  subcommands: Record<string, Cmd>;
  hide?: boolean;
}

interface ReleaseMetadata {
  version?: string;
  releasedAt?: string;
}

function getCommands(cmd: Cmd): string[][] {
  const commands: string[][] = [];
  for (const [name, sub] of Object.entries(cmd.subcommands)) {
    if (sub.hide) continue;
    commands.push(sub.full_cmd);
    commands.push(...getCommands(sub));
  }
  return commands;
}

const configDir = dirname(fileURLToPath(import.meta.url));
const cargoToml = readFileSync(resolve(configDir, "../../Cargo.toml"), "utf8");
const versionMatch = cargoToml.match(
  /\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/,
);
const aubeVersion = versionMatch?.[1] ?? "0.0.0";
const releaseMetadata = JSON.parse(
  readFileSync(resolve(configDir, "../../release.json"), "utf8"),
) as ReleaseMetadata;
const aubeReleasedAt =
  releaseMetadata.version === aubeVersion
    ? (releaseMetadata.releasedAt ?? "")
    : "";
const siteUrl = "https://aube.sh";
const siteDescription =
  "A Node.js package manager written in Rust. Run scripts with automatic installs, share dependencies across projects, and keep your existing lockfile.";

export default defineConfig({
  title: "aube",
  description: siteDescription,
  appearance: true,
  head: [
    [
      "script",
      {},
      `(function () {
  try {
    var d = document.documentElement;
    var c = JSON.parse(localStorage.getItem("jdx-banner-cache") || "null");
    var expires = c && c.expires ? Date.parse(c.expires) : NaN;
    var now = Date.now();
    var metadataValid =
      c &&
      typeof c.id === "string" &&
      typeof c.height === "string" &&
      /^[1-9]\\d*(?:\\.\\d+)?px$/.test(c.height) &&
      Number.isFinite(c.width) &&
      typeof c.fontSize === "string" &&
      Number.isFinite(c.pixelRatio) &&
      Number.isFinite(c.cachedAt) &&
      c.cachedAt <= now &&
      now - c.cachedAt < 300000 &&
      (!c.expires || (typeof c.expires === "string" && Number.isFinite(expires) && now < expires));
    var contextMatches =
      metadataValid &&
      c.width === innerWidth &&
      c.fontSize === getComputedStyle(d).fontSize &&
      c.pixelRatio === devicePixelRatio;
    if (contextMatches && localStorage.getItem("jdx-banner-dismissed") !== c.id)
      d.style.setProperty("--vp-layout-top-height", c.height);
    else if (c && !metadataValid)
      localStorage.removeItem("jdx-banner-cache");
  } catch (e) {}
})();`,
    ],
    ["link", { rel: "icon", href: "/favicon.svg", type: "image/svg+xml" }],
    ["link", { rel: "icon", href: "/favicon.ico", sizes: "any" }],
    [
      "link",
      {
        rel: "icon",
        href: "/favicon-16x16.png",
        type: "image/png",
        sizes: "16x16",
      },
    ],
    [
      "link",
      {
        rel: "icon",
        href: "/favicon-32x32.png",
        type: "image/png",
        sizes: "32x32",
      },
    ],
    [
      "link",
      {
        rel: "apple-touch-icon",
        href: "/apple-touch-icon.png",
        sizes: "180x180",
      },
    ],
    ["link", { rel: "manifest", href: "/site.webmanifest" }],
    ["meta", { name: "theme-color", content: "#FFB13B" }],
  ],
  transformHead({ pageData, siteConfig }) {
    const title =
      pageData.relativePath === "index.md"
        ? "aube — fast Node.js package management"
        : pageData.title;
    const description = pageData.description || siteDescription;
    const card = socialCard(
      pageData.relativePath === "index.md"
        ? "Fast Node.js package management"
        : pageData.title,
    );
    writeSocialCard(siteConfig.outDir, card);
    const image = new URL(card.path, `${siteUrl}/`).toString();
    const url = new URL(
      pageData.relativePath.replace(/index\.md$/, "").replace(/\.md$/, ""),
      `${siteUrl}/`,
    ).toString();

    return [
      ["link", { rel: "canonical", href: url }],
      ["meta", { property: "og:type", content: "website" }],
      ["meta", { property: "og:site_name", content: "aube" }],
      ["meta", { property: "og:locale", content: "en_US" }],
      ["meta", { property: "og:url", content: url }],
      ["meta", { property: "og:title", content: title }],
      ["meta", { property: "og:description", content: description }],
      ["meta", { property: "og:image", content: image }],
      ["meta", { property: "og:image:width", content: "1200" }],
      ["meta", { property: "og:image:height", content: "630" }],
      ["meta", { property: "og:image:alt", content: `${title} — aube` }],
      ["meta", { name: "twitter:card", content: "summary_large_image" }],
      ["meta", { name: "twitter:site", content: "@jdxcode" }],
      ["meta", { name: "twitter:title", content: title }],
      ["meta", { name: "twitter:description", content: description }],
      ["meta", { name: "twitter:image", content: image }],
      ["meta", { name: "twitter:image:alt", content: `${title} — aube` }],
      [
        "script",
        { type: "application/ld+json" },
        JSON.stringify({
          "@context": "https://schema.org",
          "@type": "WebPage",
          name: title,
          description,
          url,
          isPartOf: { "@type": "WebSite", name: "aube", url: siteUrl },
        }),
      ],
    ];
  },
  themeConfig: {
    logo: "/logo.svg",
    nav: [
      {
        text: "Docs",
        link: "/getting-started",
        activeMatch:
          "^/(guide|getting-started|installation|package-manager|.*-users|embedding|contributing|troubleshooting)",
      },
      {
        text: "Reference",
        items: [
          { text: "Commands", link: "/cli/" },
          { text: "Settings", link: "/settings/" },
          { text: "Error codes", link: "/error-codes" },
        ],
      },
      { text: "Security", link: "/security" },
      { text: "Benchmarks", link: "/benchmarks" },
      {
        text: "Community",
        items: [
          { text: "Team", link: "/team" },
          { text: "Contributing", link: "/contributing" },
          {
            text: "Discussions",
            link: "https://github.com/jdx/aube/discussions",
          },
          { text: "Releases", link: "https://github.com/jdx/aube/releases" },
        ],
      },
    ],

    sidebar: [
      {
        text: "Start here",
        items: [
          { text: "Getting started", link: "/getting-started" },
          { text: "Installation", link: "/installation" },
          { text: "Documentation map", link: "/guide" },
        ],
      },
      {
        text: "Switch to aube",
        collapsed: true,
        items: [
          { text: "From pnpm", link: "/pnpm-users" },
          { text: "From npm", link: "/npm-users" },
          { text: "From Yarn", link: "/yarn-users" },
          { text: "From Bun", link: "/bun-users" },
        ],
      },
      {
        text: "Everyday workflows",
        items: [
          { text: "Scripts and binaries", link: "/package-manager/scripts" },
          {
            text: "Manage dependencies",
            link: "/package-manager/dependencies",
          },
          { text: "Install dependencies", link: "/package-manager/install" },
          { text: "Workspaces", link: "/package-manager/workspaces" },
          {
            text: "Node runtime switching",
            link: "/package-manager/node-runtime",
          },
          { text: "CI and containers", link: "/package-manager/ci" },
          { text: "Publishing", link: "/package-manager/publishing" },
        ],
      },
      {
        text: "Configuration and storage",
        collapsed: true,
        items: [
          { text: "Configuration", link: "/package-manager/configuration" },
          {
            text: "Registry and authentication",
            link: "/package-manager/registry-auth",
          },
          { text: "Lockfiles", link: "/package-manager/lockfiles" },
          {
            text: "node_modules layout",
            link: "/package-manager/node-modules",
          },
          {
            text: "Global virtual store",
            link: "/package-manager/global-virtual-store",
          },
        ],
      },
      {
        text: "Security",
        collapsed: true,
        items: [
          { text: "Defaults and protections", link: "/security" },
          {
            text: "Lifecycle scripts",
            link: "/package-manager/lifecycle-scripts",
          },
          { text: "Jailed builds", link: "/package-manager/jailed-builds" },
          { text: "Trust policy downgrades", link: "/trust-policy-exceptions" },
          {
            text: "Security scanner",
            link: "/package-manager/security-scanner",
          },
        ],
      },
      {
        text: "Help and reference",
        items: [
          { text: "Troubleshooting", link: "/troubleshooting" },
          { text: "Error and warning codes", link: "/error-codes" },
          { text: "Settings reference", link: "/settings/" },
          { text: "Benchmarks", link: "/benchmarks" },
        ],
      },
      {
        text: "CLI reference",
        link: "/cli/",
        collapsed: true,
        items: Object.entries(spec.cmd.subcommands)
          .filter(([, cmd]) => !cmd.hide)
          .map(([name, cmd]) => {
            const children = getCommands(cmd as unknown as Cmd);
            return {
              text: `aube ${name}`,
              link: `/cli/${name}`,
              ...(children.length
                ? {
                    collapsed: true,
                    items: children.map((child) => ({
                      text: child.join(" "),
                      link: `/cli/${child.join("/")}`,
                    })),
                  }
                : {}),
            };
          }),
      },
      {
        text: "Embedding",
        collapsed: true,
        items: [
          { text: "Choose an integration", link: "/embedding/" },
          { text: "Rust", link: "/embedding/rust" },
          { text: "Node-API", link: "/embedding/node" },
          { text: "C ABI", link: "/embedding/ffi" },
        ],
      },
      {
        text: "Project",
        collapsed: true,
        items: [
          { text: "Contributing", link: "/contributing" },
          { text: "Team", link: "/team" },
        ],
      },
    ],

    outline: { level: [2, 3] },

    footer: false,

    editLink: {
      pattern: "https://github.com/jdx/aube/edit/main/docs/:path",
      text: "Edit this page on GitHub",
    },

    search: { provider: "local" },
  },
  vite: {
    define: {
      __AUBE_VERSION__: JSON.stringify(aubeVersion),
      __AUBE_RELEASED_AT__: JSON.stringify(aubeReleasedAt),
    },
    plugins: [
      {
        name: "aube-version-file",
        closeBundle() {
          const distDir = resolve(configDir, "dist");
          mkdirSync(distDir, { recursive: true });
          writeFileSync(resolve(distDir, "VERSION"), `${aubeVersion}\n`);
        },
      },
    ],
  },
});
