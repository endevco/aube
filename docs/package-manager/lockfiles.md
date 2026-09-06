---
description: Check supported lockfile formats, file precedence, frozen installs, conversion, and Node runtime pins.
---

# Lockfiles

aube's default lockfile for new projects is `aube-lock.yaml`. For projects
that already have a different supported lockfile, aube keeps reading and
writing that file in place.

## Supported lockfile formats

aube reads *and writes* all of the following formats:

| File | Supported format | Before switching |
| --- | --- | --- |
| `aube-lock.yaml` | aube's native YAML format | Default for a new project |
| `pnpm-lock.yaml` | v9 | Upgrade v5/v6 lockfiles with pnpm first |
| `package-lock.json` | v2 and v3 | Keep the file in place |
| `npm-shrinkwrap.json` | npm shrinkwrap | Takes precedence over `package-lock.json` |
| `yarn.lock` | Classic v1 and Berry v2+ | PnP projects need a `node_modules` linker |
| `bun.lock` | Text format v1 | Convert binary `bun.lockb` with Bun first |

## Write behavior

On install (and on `add`, `remove`, `update`, `dedupe`), aube picks the
lockfile to write from whichever supported file already exists in the project
directory. Precedence is: `aube-lock.yaml` → `pnpm-lock.yaml` → `bun.lock` →
`yarn.lock` → `npm-shrinkwrap.json` → `package-lock.json`. When none of those
exist yet, aube writes `aube-lock.yaml` by default;
`default-lockfile-format` can select `pnpm-lock.yaml`.

For example:

- A pnpm project keeps getting `pnpm-lock.yaml` updates.
- An npm project keeps getting `package-lock.json` updates.
- `aube import` switches a project onto `aube-lock.yaml`; removing the
  existing lockfile makes the next install follow the configured default.

To create `pnpm-lock.yaml` when a project has no lockfile yet, set the
creation default in `.npmrc`:

```ini
default-lockfile-format=pnpm
```

This also preserves the pnpm filename after `aube clean --lockfile` followed
by `aube install`. The setting does not convert an existing lockfile; the
existing supported file still wins.

Keep one canonical lockfile while both tools are in use. Review lockfile
diffs as you would with the original package manager; preserving the format
does not prevent merge conflicts or guarantee identical version selection.

## Convert intentionally

`aube import` reads an existing supported lockfile and writes `aube-lock.yaml`.
Use it only when you want to change formats. The new file takes precedence;
retire the previous lockfile once all workflows use the new one.

## Frozen installs

```sh
aube install --frozen-lockfile
aube ci
```

Frozen mode fails when the lockfile no longer matches the manifest.

## Prefer frozen installs

```sh
aube install --prefer-frozen-lockfile
```

This is the local default. aube uses the lockfile if it is fresh and
re-resolves when the manifest changed.

## Lockfile-only updates

```sh
aube install --lockfile-only
```

Use this when CI or automation needs to update dependency metadata without
touching `node_modules`.

## Runtime pins

When `package.json` pins Node through `devEngines.runtime`, the
resolved exact version (plus per-platform download URLs and SHA-256
checksums) is recorded in the lockfile using pnpm 10.14+'s
`node@runtime:` entry shape — a synthetic dep on the root importer and
a `packages:` entry with a `variations` resolution. aube and pnpm read
each other's pins. Formats without a runtime shape (npm / yarn / bun)
skip the pin and re-resolve the range at run time. See
[Node runtime switching](/package-manager/node-runtime).

## Branch lockfiles

When `gitBranchLockfile` is enabled, aube writes branch-specific lockfile names
such as `aube-lock.<branch>.yaml`. Use this for long-running branches that
produce frequent lockfile conflicts.
