---
description: Install aube, run your first script, and review dependency builds in an existing Node.js project.
---

# Getting started

You need a Node.js project with a `package.json`. aube can use your existing
pnpm, npm, Yarn, or Bun text lockfile; no conversion is needed to try it.

## 1. Install aube

```sh
mise use -g aube
aube --version
```

This installs aube globally with [mise](https://mise.jdx.dev).
See [installation](/installation) for Homebrew, npm, Cargo, and Linux packages.

## 2. Run a project script

From your project directory, run a script that exists in `package.json`:

```sh
aubr build
```

`aubr` means `aube run`. It installs missing or stale dependencies, then starts
the script. When the manifest, lockfile, and install settings are unchanged,
the next run goes straight to the script.

<TerminalPreview />

To install without starting a script, use `aube install`. To start a new
project, run `aube init`, then add the dependencies you need.

## 3. Review dependency builds

Root lifecycle scripts run normally. Dependency lifecycle scripts need project
approval or aube's built-in trust; an explicit deny always wins. If a native
package is missing its build output, inspect the skipped scripts:

```sh
aube ignored-builds
aube approve-builds
aube rebuild
```

Approve only packages you have reviewed. Commit the resulting `allowBuilds`
policy along with any manifest and lockfile changes.
See [lifecycle scripts](/package-manager/lifecycle-scripts) for policy details.

## 4. Keep working

| What you need | Command |
| --- | --- |
| Add a runtime dependency | `aube add react` |
| Add a test tool | `aube add -D vitest` |
| Run the test script | `aube test` |
| Run an installed tool | `aube exec vitest` |
| Run a one-off tool | `aubx cowsay hi` |
| Install from a committed lockfile in CI | `aube ci` |

`aubx` means `aube dlx`: it uses a matching local binary when available,
otherwise installs the tool in a throwaway project.

## Before switching your team

Review the lockfile diff and run your project's tests. aube preserves supported
lockfile formats, but its isolated dependency layout and security defaults can
expose assumptions in an existing project.

Choose your migration guide: [pnpm](/pnpm-users), [npm](/npm-users),
[Yarn](/yarn-users), or [Bun](/bun-users). Then set up
[CI](/package-manager/ci) and explore the [documentation guide](/guide).
