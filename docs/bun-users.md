# For bun users

aube can install directly from Bun lockfiles. You do not need to delete
`bun.lock` or remove `node_modules` before trying aube.

## Start from the Bun lockfile

```sh
aube install
```

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
project to `aube-lock.yaml`. After import succeeds, remove `bun.lock` so
future installs keep writing `aube-lock.yaml`.

## Differences from Bun

- aube keeps package files in a global content-addressable store.
- aube produces an isolated symlink layout under `node_modules/.aube/`
  rather than Bun's hoisted tree.
- aube does not manage a JavaScript runtime. Use
  [mise](https://mise.jdx.dev) (`mise use node@22`) if you need a Node
  version alongside or in place of Bun.
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`)
  follow the pnpm v11 allowlist rather than Bun's `trustedDependencies`
  list. aube runs them only for packages you've explicitly allowlisted
  via `pnpm.allowBuilds`, `pnpm.onlyBuiltDependencies`, or
  `aube approve-builds`. aube does not read `trustedDependencies`, so
  anything you had listed there needs to be moved to one of the pnpm
  fields before aube will run its scripts.

Reference: [bun install](https://bun.sh/docs/cli/install)
