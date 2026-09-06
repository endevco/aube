import { readFile, readdir, writeFile } from "node:fs/promises";

// usage-lib does not model relationships between flags, so correct generated
// usage strings whose optionality depends on another flag.
const replacements = [
  {
    path: "docs/cli/index.md",
    from: "aube store prune [--dry-run] [--json]",
    to: "aube store prune [--dry-run [--json]]",
  },
  {
    path: "docs/cli/store.md",
    from: "aube store prune [--dry-run] [--json]",
    to: "aube store prune [--dry-run [--json]]",
  },
  {
    path: "docs/cli/store/prune.md",
    from: "aube store prune [--dry-run] [--json]",
    to: "aube store prune [--dry-run [--json]]",
  },
  {
    path: "docs/cli/commands.json",
    from: '"usage": "store prune [--dry-run] [--json]"',
    to: '"usage": "store prune [--dry-run [--json]]"',
  },
];

for (const { path, from, to } of replacements) {
  const source = await readFile(path, "utf8");
  const occurrences = source.split(from).length - 1;
  if (occurrences === 0 && source.includes(to)) continue;
  if (occurrences !== 1) {
    throw new Error(
      `expected exactly one generated usage string in ${path}, found ${occurrences}`,
    );
  }
  await writeFile(path, source.replace(from, to));
}

// Keep task navigation and worked examples outside the CLI flag declarations.
// This postprocessor runs locally and in the documentation build workflow.
const groups = [
  {
    title: "Install and change dependencies",
    guide: "/package-manager/dependencies",
    commands: [
      "install",
      "ci",
      "add",
      "remove",
      "update",
      "dedupe",
      "prune",
      "fetch",
      "import",
      "init",
      "link",
      "unlink",
      "patch",
      "patch-commit",
      "patch-remove",
      "clean",
      "purge",
    ],
  },
  {
    title: "Run scripts and tools",
    guide: "/package-manager/scripts",
    commands: [
      "run",
      "exec",
      "dlx",
      "create",
      "test",
      "start",
      "stop",
      "restart",
    ],
  },
  {
    title: "Inspect and troubleshoot",
    guide: "/troubleshooting",
    commands: [
      "view",
      "list",
      "why",
      "outdated",
      "query",
      "peers",
      "check",
      "doctor",
      "diag",
      "deprecations",
      "bugs",
    ],
  },
  {
    title: "Review builds and security",
    guide: "/security",
    commands: [
      "approve-builds",
      "ignored-builds",
      "rebuild",
      "audit",
      "trust",
      "licenses",
      "sbom",
    ],
  },
  {
    title: "Publish packages",
    guide: "/package-manager/publishing",
    commands: [
      "pack",
      "publish",
      "version",
      "dist-tag",
      "access",
      "deprecate",
      "undeprecate",
      "unpublish",
    ],
  },
  {
    title: "Configure projects and runtimes",
    guide: "/package-manager/configuration",
    commands: [
      "config",
      "runtime",
      "node",
      "activate",
      "completion",
      "login",
      "logout",
      "recursive",
      "deploy",
    ],
  },
  {
    title: "Inspect stores and paths",
    guide: "/package-manager/node-modules",
    commands: [
      "store",
      "cache",
      "cat-file",
      "cat-index",
      "find-hash",
      "bin",
      "root",
      "prefix",
    ],
  },
  { title: "Project", guide: "/team", commands: ["sponsors"] },
];
const guides = {
  install: "/package-manager/install",
  ci: "/package-manager/ci",
  fetch: "/package-manager/ci",
  import: "/package-manager/lockfiles",
  runtime: "/package-manager/node-runtime",
  node: "/package-manager/node-runtime",
  activate: "/package-manager/node-runtime",
  completion: "/installation#shell-completions",
  login: "/package-manager/registry-auth",
  logout: "/package-manager/registry-auth",
  cache: "/package-manager/registry-auth",
  recursive: "/package-manager/workspaces",
  deploy: "/package-manager/workspaces",
  "approve-builds": "/package-manager/lifecycle-scripts",
  "ignored-builds": "/package-manager/lifecycle-scripts",
  rebuild: "/package-manager/lifecycle-scripts",
  trust: "/trust-policy-exceptions",
};
const examples = {
  install: [
    "aube install",
    "aube install --frozen-lockfile",
    "aube install --prod",
  ],
  ci: ["aube ci", "aube run --no-install test"],
  add: [
    "aube add react",
    "aube add -D vitest",
    "aube --filter @acme/app add zod",
  ],
  remove: ["aube remove react"],
  update: ["aube update", "aube update --latest react"],
  dedupe: ["aube dedupe --check", "aube dedupe"],
  prune: ["aube prune --prod"],
  fetch: ["aube fetch --prod"],
  import: ["aube import"],
  init: ["aube init --init-type module"],
  run: [
    "aube run build",
    "aube run --no-install test --watch",
    "aube -r run build",
  ],
  exec: ["aube exec vitest", "aube exec tsc -- --noEmit"],
  dlx: ["aubx cowsay hi", "aubx --package create-vite create-vite my-app"],
  create: ["aube create vite my-app"],
  test: ["aube test", "aube test --no-install"],
  start: ["aube start"],
  stop: ["aube stop"],
  restart: ["aube restart"],
  view: ["aube view react version", "aube view react --json"],
  list: ["aube list --depth 0", "aube list --json"],
  outdated: ["aube outdated", "aube outdated --json"],
  query: ["aube query ':scripts'", "aube query '[name=react]' --json"],
  check: ["aube check --json"],
  doctor: ["aube doctor", "aube doctor --json"],
  diag: ["aube --diag full install", "aube diag analyze aube-diag.jsonl"],
  "diag/analyze": ["aube diag analyze aube-diag.jsonl"],
  "diag/compare": ["aube diag compare baseline.jsonl current.jsonl"],
  audit: ["aube audit --audit-level high", "aube audit --json"],
  "approve-builds": [
    "aube ignored-builds",
    "aube approve-builds",
    "aube rebuild",
  ],
  "ignored-builds": ["aube ignored-builds"],
  rebuild: ["aube rebuild"],
  trust: ["aube trust check react@19.1.0"],
  "trust/check": ["aube trust check react@19.1.0 --json"],
  licenses: ["aube licenses --prod", "aube licenses --json"],
  sbom: [
    "aube sbom --format cyclonedx",
    "aube sbom --lockfile-only --format spdx",
  ],
  pack: ["aube pack --dry-run", "aube pack --pack-destination dist"],
  publish: ["aube publish --dry-run --json", "aube publish --access public"],
  version: ["aube version", "aube version patch --no-git-tag-version"],
  "dist-tag": ["aube dist-tag ls @acme/widget"],
  "dist-tag/ls": ["aube dist-tag ls @acme/widget"],
  "dist-tag/add": ["aube dist-tag add @acme/widget@1.2.0 next"],
  "dist-tag/rm": ["aube dist-tag rm @acme/widget next"],
  unpublish: ["aube unpublish @acme/widget@1.2.0 --dry-run"],
  config: ["aube config find registry", "aube config explain nodeLinker"],
  "config/find": ["aube config find registry"],
  "config/explain": ["aube config explain minimumReleaseAge"],
  "config/get": ["aube config get registry"],
  "config/list": ["aube config list --json"],
  "config/set": ["aube config set --local nodeLinker hoisted"],
  "config/delete": ["aube config delete --local nodeLinker"],
  "config/tui": ["aube config tui"],
  runtime: ["aube runtime list", "aube runtime set node 24 --save-exact"],
  "runtime/list": ["aube runtime list"],
  "runtime/set": ["aube runtime set node 24 --save-exact"],
  node: ["aube node --version"],
  activate: ["aube activate zsh"],
  completion: ["aube completion zsh --install"],
  login: ["aube login --scope @acme --registry https://registry.example.test/"],
  logout: ["aube logout --scope @acme"],
  deploy: ["aube --filter @acme/api deploy dist/api"],
  store: ["aube store path", "aube store status", "aube store prune --dry-run"],
  "store/path": ["aube store path"],
  "store/status": ["aube store status"],
  "store/prune": [
    "aube store prune --dry-run",
    "aube store prune --dry-run --json",
  ],
  "store/add": ["aube store add react@19.1.0"],
  cache: ["aube cache path", "aube cache view react"],
  "cache/path": ["aube cache path"],
  "cache/view": ["aube cache view react --json"],
  "cache/list": ["aube cache list '@babel/*'"],
  "cache/delete": ["aube cache delete '@babel/*'"],
  "cache/prune": ["aube cache prune --dry-run"],
  "cache/list-registries": ["aube cache list-registries"],
  "cat-index": ["aube cat-index react@19.1.0"],
  bin: ["aube bin", "aube bin --global"],
  root: ["aube root"],
  prefix: ["aube prefix"],
  clean: ["aube clean"],
  purge: ["aube purge"],
};
const spec = JSON.parse(await readFile("docs/cli/commands.json", "utf8"));
const commandNames = Object.entries(spec.cmd.subcommands)
  .filter(([, command]) => !command.hide)
  .map(([name]) => name);
for (const name of commandNames) {
  if (!groups.some((group) => group.commands.includes(name))) {
    throw new Error(
      `Add the new command ${name} to the documentation navigation groups`,
    );
  }
}
const pages = (await readdir("docs/cli", { recursive: true }))
  .filter((path) => path.endsWith(".md"))
  .sort();
for (const page of pages) {
  const path = `docs/cli/${page}`;
  const key = page.replace(/\.md$/, "").replaceAll("\\", "/");
  const command = key.split("/")[0];
  let source = await readFile(path, "utf8");
  // Make reruns safe: remove only blocks owned by this postprocessor.
  source = source
    .replace(/^---\n[\s\S]*?\n---\n\n/, "")
    .replace(
      /<!-- docs-navigation:start -->[\s\S]*?<!-- docs-navigation:end -->\n*/g,
      "",
    )
    .replace(/^\*\*Usage:\*\*[^\n]+\n\n(?=- \*\*Usage:)/m, "")
    .replace(/^(##[^\n]+)\n(?=\S)/gm, "$1\n\n");
  const intro = source.match(
    /(?:^- \*\*(?:Usage|Aliases|Effect):[^\n]+\n)+\n([^#][^\n]*)/m,
  )?.[1];
  const description =
    key === "index"
      ? "Find aube commands by task, with examples, arguments, global options, and links to workflow guides."
      : (
          intro || `Arguments and options for aube ${key.replaceAll("/", " ")}.`
        ).replaceAll("`", "");
  source = `---\ndescription: ${JSON.stringify(description)}\neditLink: false\n---\n\n${source}`;
  if (key === "index") {
    source = source.replace("# `aube`", "# CLI reference");
    const navigation = groups
      .map((group) => {
        const links = group.commands
          .filter((name) => commandNames.includes(name))
          .map((name) => "[`" + name + "`](/cli/" + name + ")")
          .join(" · ");
        return `| ${group.title} | ${links} |`;
      })
      .join("\n");
    const block = `<!-- docs-navigation:start -->\n\nRun \`aube <command> --help\` for help in your terminal. \`aubr\` is shorthand for\n\`aube run\`; \`aubx\` is shorthand for \`aube dlx\`. For a guided introduction,\nstart with [getting started](/getting-started).\n\n## Find a command\n\n| Task | Commands |\n| --- | --- |\n${navigation}\n\nThe reference records supported flags and calls out compatibility flags with\nlimited behavior. See [pnpm differences](/pnpm-users) before replacing a workflow.\n\n<!-- docs-navigation:end -->\n\n`;
    source = source.replace("## Global Flags", `${block}## Global Flags`);
  } else {
    const group = groups.find((entry) => entry.commands.includes(command));
    const guide = guides[command] || group?.guide || "/guide";
    // usage's after-help output is plain terminal text; preserve its layout.
    source = source.replace(
      /\nExamples:\n\n([\s\S]*)$/,
      (_, text) =>
        `\n## Examples\n\n\`\`\`console\n${text.trimEnd().replace(/^  /gm, "")}\n\`\`\`\n`,
    );
    const example =
      examples[key] && !source.includes("## Examples")
        ? `## Examples\n\n\`\`\`sh\n${examples[key].join("\n")}\n\`\`\`\n\n`
        : "";
    const block = `<!-- docs-navigation:start -->\n\n${example}Read the [workflow guide](${guide}) for context.\n[Global options](/cli/#global-flags) apply to this command too.\n\n<!-- docs-navigation:end -->\n\n`;
    const firstSection = source.indexOf("\n## ");
    if (firstSection < 0) source += `\n${block}`;
    else
      source =
        source.slice(0, firstSection + 1) +
        block +
        source.slice(firstSection + 1);
  }
  await writeFile(path, source);
}
console.log(`Enhanced ${pages.length} CLI reference pages`);
