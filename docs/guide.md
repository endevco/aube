---
description: Find aube tutorials, migration guides, package-management workflows, and command and settings references.
---

# Documentation

aube is a Node.js package manager written in Rust. It installs dependencies
before you run scripts, shares package files across projects, and preserves
supported lockfiles in their original format.

New to aube? Follow [getting started](/getting-started) to install it and run
your first project script.

## Set up your project

| I want to… | Read |
| --- | --- |
| Install or update aube | [Installation](/installation) |
| Try it in an existing project | [pnpm](/pnpm-users), [npm](/npm-users), [Yarn](/yarn-users), or [Bun](/bun-users) |
| Run builds, tests, and tools | [Scripts and binaries](/package-manager/scripts) |
| Set up a monorepo | [Workspaces](/package-manager/workspaces) |
| Pin Node.js | [Node runtime switching](/package-manager/node-runtime) |
| Configure CI or a container | [CI and containers](/package-manager/ci) |

## Manage dependencies

- [Install dependencies](/package-manager/install): lockfile modes, offline installs, and package layouts.
- [Manage dependencies](/package-manager/dependencies): add, remove, update, dedupe, and prune.
- [Lockfiles](/package-manager/lockfiles): supported formats, precedence, and conversion.
- [Configuration](/package-manager/configuration): choose where a setting belongs and inspect its effective value.
- [Registry and authentication](/package-manager/registry-auth): private packages, tokens, proxies, and TLS.
- [Publishing](/package-manager/publishing): inspect a package, publish it, and manage releases.

## Understand the defaults

Dependency scripts require an allow rule or built-in trust. Resolution checks
release age and publishing evidence. [Security](/security) explains which checks
warn, which block, and which need an explicit opt-in.

The default layout isolates dependencies. A global content store shares package
files, while the global virtual store also shares directory trees for local
installs. Read about [node_modules](/package-manager/node-modules),
the [global virtual store](/package-manager/global-virtual-store), and the
[benchmarks](/benchmarks).

## Look up a detail

- [CLI reference](/cli/): commands, arguments, and flags.
- [Settings reference](/settings/): defaults and supported configuration sources.
- [Error and warning codes](/error-codes): stable identifiers and exit codes.
- [Troubleshooting](/troubleshooting): symptoms, diagnostics, and targeted fixes.
- [Embedding](/embedding/): Rust, Node-API, and C ABI integration.

To help improve aube, see [contributing](/contributing) or start a
[Discussion](https://github.com/jdx/aube/discussions).
