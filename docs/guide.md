# Guide

aube is a package manager for Node.js projects. It uses pnpm-style isolated
`node_modules` for fast, disk-efficient installs.

Existing projects keep their lockfile format. aube reads and writes
`pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and
`bun.lock` in place. New projects without a supported lockfile get
`aube-lock.yaml`.

::: info Name
`aube` means dawn in French. It is pronounced `/ob/`.
:::

## Start here

- [Installation](/installation) shows the recommended mise install path,
  source builds, and shell completions.
- For existing projects, see the [pnpm](/pnpm-users), [npm](/npm-users),
  or [yarn](/yarn-users) guide.
- [Install dependencies](/package-manager/install) covers the normal install
  path, CI mode, production installs, offline installs, and lockfile modes.
- [Manage dependencies](/package-manager/dependencies) covers `add`, `remove`,
  `update`, `dedupe`, and `prune`.
- [Workspaces](/package-manager/workspaces) covers `aube-workspace.yaml`,
  workspace linking, filters, recursive runs, catalogs, and deploys.

## Package-manager model

aube has the same CLI, config, and internals that pnpm v11 does.

- A strict, isolated `node_modules` layout.
- A content-addressable global store.
- A shared lockfile for workspaces.
- `workspace:`, `link:`, `file:`, git, tarball URL, npm alias, and catalog
  dependency specifiers.
- Root lifecycle scripts, with dependency lifecycle scripts gated by an
  explicit allowlist.

aube uses its own internal directory names: `node_modules/.aube/` for the
virtual store and `~/.aube-store/` for the global store. Existing lockfiles
are preserved in place; only projects with no supported lockfile yet start
with `aube-lock.yaml`.

## Reference sections

- [CLI Reference](/cli/) is generated from the command parser.
- [Settings Reference](/settings/) is generated from `settings.toml`.
- [pnpm Compatibility](/pnpm-compatibility) is the high-level parity matrix.
- [Benchmarks](/benchmarks) explains the performance measurements.
