# Migrating projects

aube can install directly from the lockfiles your project already has. That is
the migration path: bring aube into an existing pnpm, npm, Yarn (classic or
berry), or Bun project and run one command. You do not need to delete the old
lockfile, throw away `node_modules`, or manually translate dependency metadata
before trying aube.

This is one of aube's main compatibility guarantees. aube reads the existing
lockfile and writes updates back to the same file, so teams can test aube
without forcing every contributor or CI job to switch at the same time.

## Pick your starting point

| Existing project | Lockfile aube reads | Migration guide |
| --- | --- | --- |
| pnpm | `pnpm-lock.yaml` v9 | [For pnpm users](/pnpm-users) |
| npm | `package-lock.json`, `npm-shrinkwrap.json` | [npm migration](/npm-migration) |
| Yarn classic | `yarn.lock` v1 | [Yarn migration](/yarn-migration) |
| Yarn berry | `yarn.lock` v2+ | [Yarn migration](/yarn-migration) |
| Bun | `bun.lock` | This page |

Every path starts the same way:

```sh
aube install
```

## What aube writes

After reading an existing lockfile, aube:

- Updates the existing lockfile in place (`pnpm-lock.yaml`,
  `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, or `bun.lock`) —
  no surprise `aube-lock.yaml` appears alongside it. Projects with no
  lockfile yet get `aube-lock.yaml` as the default.
- Leaves any existing `pnpm-workspace.yaml` in place. aube does not emit
  an `aube-workspace.yaml` for you; new projects that want workspace
  metadata can opt in by creating the file themselves.
- Installs into `node_modules/.aube/` (the per-project virtual store).
- Populates `~/.aube-store/v1/files/` (the global content-addressable store).

The original lockfile keeps working with the original package manager, so a
team can try aube gradually.

## Bun projects

aube reads and writes Bun's text `bun.lock` format. Keep `bun.lock` while Bun
remains in use. Bun and aube can coexist during a transition because aube uses
its own workspace metadata and virtual store.

## Rollout checklist

- Run `aube install` against the existing lockfile.
- Commit the updated lockfile (whichever kind the project already uses) so
  aube users and the original package manager both get reproducible installs.
- Update one CI job or Docker build to use `aube install` or `aube ci`.
- Run `aube test` and the workspace scripts that matter for the project.
- If the team chooses to convert the project to aube's own lockfile later, run
  `aube import`. It reads the existing lockfile (one of `pnpm-lock.yaml`,
  `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, or `bun.lock`)
  and writes `aube-lock.yaml`. Once the import succeeds, you can
  optionally remove the original lockfile.
