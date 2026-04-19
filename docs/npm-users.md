# For npm users

aube can install directly from npm lockfiles. You do not need to delete
`package-lock.json`, remove `node_modules`, or translate dependency ranges
before trying aube.

## Start from the npm lockfile

```sh
aube install
```

aube reads:

- `package-lock.json`
- `npm-shrinkwrap.json`

It updates whichever of those two files the project already has on disk (no
surprise `aube-lock.yaml` appears alongside it) and installs packages into
`node_modules/.aube/`.

## Keep npm working during rollout

Commit the updated `package-lock.json` (or `npm-shrinkwrap.json`) so both
npm and aube users see the same resolved versions. You do not need
`aube import` for a normal rollout; `aube install` keeps the npm lockfile as
the shared source of truth.

Use `aube import` only if the team intentionally wants to convert the project
to `aube-lock.yaml`. After import succeeds, remove the npm lockfile so future
installs keep writing `aube-lock.yaml`.

## Differences from npm

- aube's default `node_modules` layout is
  [isolated](/package-manager/node-modules), not flat.
- Only declared direct dependencies appear at the project top level,
  unless you opt into
  [`nodeLinker: hoisted`](/settings/#setting-nodelinker).
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`) do
  not run by default. npm runs them for every dependency; aube runs them
  only for packages you've explicitly allowlisted via `pnpm.allowBuilds`,
  `pnpm.onlyBuiltDependencies`, or `aube approve-builds`. This follows
  the pnpm v11 model.
- Global installs live under aube's global package directory instead of npm's
  shared global `node_modules`.

Reference: [npm install](https://docs.npmjs.com/cli/v10/commands/npm-install)
