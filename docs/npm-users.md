---
description: Use aube with package-lock.json or npm-shrinkwrap.json and account for isolated dependencies and build approvals.
---

# For npm users

aube can install directly from npm lockfiles. You do not need to delete
`package-lock.json` or remove `node_modules` before trying aube.

## Try the npm lockfile

```sh
aube install
```

Run this from the project root, then review the lockfile diff and run your
tests. For daily work, `aubr build`, `aube test`, and `aube exec <bin>`
install automatically when dependencies are stale. Use `aubx <pkg>` for
one-off tools.

aube reads:

- `package-lock.json` v2 or v3
- `npm-shrinkwrap.json`

If both files exist, `npm-shrinkwrap.json` takes precedence. aube updates the
selected file and
installs packages into `node_modules/.aube/`.

## Keep npm working during rollout

Commit the updated `package-lock.json` (or `npm-shrinkwrap.json`) so both
npm and aube users see the same resolved versions. You do not need
`aube import` for a normal rollout; `aube install` keeps the npm lockfile as
the shared source of truth.

Use `aube import` only if the team intentionally wants to convert the project
to `aube-lock.yaml`. The new `aube-lock.yaml` takes precedence on later installs. Retire the old
lockfile once the team has switched, to avoid maintaining two sources of truth.

## Differences from npm

- aube's default `node_modules` layout is
  [isolated](/package-manager/node-modules), not flat.
- Only declared direct dependencies appear at the project top level,
  unless you opt into
  [`nodeLinker: hoisted`](/settings/#setting-nodelinker).
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`)
  run only when project policy or aube's built-in trusted-dependencies list
  allows them. Explicit denies win. Legacy `pnpm.onlyBuiltDependencies`
  entries are still honored. Approved dependency builds can also run in a
  [jail](/package-manager/jailed-builds) with package-specific env, path,
  and network permissions.
- Global installs live under aube's global package directory instead of npm's
  shared global `node_modules`.

Reference: [npm install](https://docs.npmjs.com/cli/v10/commands/npm-install)

## Next steps

See [lifecycle scripts](/package-manager/lifecycle-scripts) for build approvals,
[CI and containers](/package-manager/ci) for reproducible installs, and
[troubleshooting](/troubleshooting) for compatibility problems.
