---
description: Use aube with Bun text lockfiles, preserve trusted dependency approvals, and understand Node runtime differences.
---

# For Bun users

aube can install directly from Bun lockfiles. You do not need to delete
`bun.lock` or remove `node_modules` before trying aube.

## Try the Bun lockfile

```sh
aube install
```

Run this from the project root, then review the lockfile diff and run your
tests. For daily work, `aubr build`, `aube test`, and `aube exec <bin>`
install automatically when dependencies are stale. Use `aubx <pkg>` for
one-off tools.

aube reads and updates the text-format `bun.lock` at `lockfileVersion: 1`
in place and installs packages into `node_modules/.aube/`.

aube does not read Bun's older binary `bun.lockb` format. Projects still
on `bun.lockb` can generate the text lockfile with a modern Bun once:

```sh
bun install --save-text-lockfile
```

Commit the resulting `bun.lock` and drop `bun.lockb` before switching to
aube.

## Keep Bun working during rollout

Commit the updated `bun.lock` so both Bun and aube users see the same
resolved versions. You do not need `aube import` for a normal rollout;
`aube install` keeps `bun.lock` as the shared source of truth.

Use `aube import` only if the team intentionally wants to convert the
project to `aube-lock.yaml`. The new `aube-lock.yaml` takes precedence on later installs. Retire the old
lockfile once the team has switched, to avoid maintaining two sources of truth.

## Differences from Bun

- aube keeps package files in a global content-addressable store.
- aube produces an isolated symlink layout under `node_modules/.aube/`
  with a shared virtual store enabled for compatible local projects.
- aube does not manage the Bun runtime, only Node (see
  [Node runtime switching](/package-manager/node-runtime)). Use
  [mise](https://mise.jdx.dev) (`mise use bun`) if you still need Bun
  alongside aube.
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`)
  are gated by an allowlist. aube reads Bun's top-level
  `trustedDependencies` array in addition to pnpm's
  `pnpm.allowBuilds` / `pnpm.onlyBuiltDependencies`, so an existing
  Bun manifest retains those approvals. Explicit aube deny rules still win.
  Install writes unreviewed packages into `aube-workspace.yaml`'s
  `allowBuilds` with `false` (or `pnpm-workspace.yaml` if one already
  exists); `aube approve-builds` flips reviewed entries to `true`. Approved dependency builds can also run in a
  [jail](/package-manager/jailed-builds) with package-specific env, path,
  and network permissions.

Reference: [bun install](https://bun.sh/docs/cli/install)

## Next steps

See [lifecycle scripts](/package-manager/lifecycle-scripts) for build approvals,
[CI and containers](/package-manager/ci) for reproducible installs, and
[troubleshooting](/troubleshooting) for compatibility problems.
