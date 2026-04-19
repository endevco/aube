# For pnpm users

aube is designed for projects that already use pnpm. Most daily commands keep
the same shape, and the differences are about aube-owned state rather than a
new package-management model.

For the full compatibility picture, including the pnpm surface aube does not
implement, see [pnpm Compatibility](/pnpm-compatibility).

## Command map

| pnpm | aube | Notes |
| --- | --- | --- |
| `pnpm install` | `aube install` | Reads and updates an existing `pnpm-lock.yaml` in place. Only new projects (no supported lockfile on disk yet) default to `aube-lock.yaml`. |
| `pnpm add react` | `aube add react` | Supports dependency sections, exact pins, peer deps, workspace root adds, and globals. |
| `pnpm remove react` | `aube remove react` | Removes from the manifest and relinks. |
| `pnpm update` | `aube update` | Updates all or named direct dependencies. |
| `pnpm run build` | `aube run build` | Runs scripts with an auto-install staleness check first. |
| `pnpm test` | `aube test` | Shortcut for the `test` script; aube auto-installs first. |
| `pnpm exec vitest` | `aube exec vitest` | Runs local binaries with project `node_modules/.bin` on `PATH`. |
| `pnpm dlx cowsay hi` | `aube dlx cowsay hi` | Installs into a throwaway environment and runs the binary. |
| `pnpm list` | `aube list` | Supports depth, JSON, parseable, long, prod/dev, and global modes. |
| `pnpm why debug` | `aube why debug` | Shows reverse dependency paths. |
| `pnpm pack` | `aube pack` | Creates a publishable tarball with npm-style file selection. |
| `pnpm publish` | `aube publish` | Publishes to the configured registry; workspace fanout is available via filters. |
| `pnpm approve-builds` | `aube approve-builds` | Records packages allowed to run lifecycle build scripts. |

## Files and directories

| Concept | pnpm | aube |
| --- | --- | --- |
| Default lockfile (new projects) | `pnpm-lock.yaml` | `aube-lock.yaml` |
| Virtual store | `node_modules/.pnpm/` | `node_modules/.aube/` |
| Global content-addressable store | `~/.pnpm-store/` | `~/.aube-store/v1/files/` |
| Install state | `node_modules/.modules.yaml` | `node_modules/.aube-state` |
| Workspace manifest | `pnpm-workspace.yaml` | `aube-workspace.yaml` |

aube reads pnpm v11 YAML files for compatibility. The aube-owned files use
compatible shapes today, but `aube-lock.yaml` and `aube-workspace.yaml` are
the long-term contract and may diverge from pnpm after the beta compatibility
push.

aube never touches pnpm's `node_modules/.pnpm/` or `~/.pnpm-store/`. The two
virtual stores can coexist under `node_modules`. For the lockfile and
workspace YAML, aube reads and writes whichever file already exists on disk
— `pnpm-lock.yaml` keeps getting updates in place, and `pnpm-workspace.yaml`
is read in place (aube does not emit an `aube-workspace.yaml` for existing
projects).

## Main aube-owned behavior

- aube installs into its own `node_modules/.aube/` and `~/.aube-store/`
  instead of pnpm's `.pnpm/` and `~/.pnpm-store/`.
- Lockfile writeback preserves the existing kind: `pnpm-lock.yaml` stays
  `pnpm-lock.yaml` after an aube install. New projects (no supported
  lockfile yet) get `aube-lock.yaml` by default.
- Workspace discovery prefers `aube-workspace.yaml` when present, falls back
  to `pnpm-workspace.yaml`. aube does not generate either file at install
  time; the one place aube writes to workspace yaml is
  `aube approve-builds`, which appends to `onlyBuiltDependencies` in
  `pnpm-workspace.yaml` (creating it if missing) for pnpm v10 parity.
- Dependency lifecycle script approval follows the pnpm v11 model through
  `pnpm.allowBuilds`, `pnpm.onlyBuiltDependencies`, or
  `pnpm.neverBuiltDependencies`.
- `aube test`, `aube run`, and `aube exec` check the install state before
  running, so scripts can repair stale installs automatically.
- Runtime-management commands such as `pnpm env`, `pnpm runtime`, `pnpm setup`,
  and `pnpm self-update` are intentionally out of scope for aube.

## Typical migration

```sh
aube install
aube test
git add pnpm-lock.yaml
```

Keep committing `pnpm-lock.yaml` (now updated by aube) while pnpm is still
part of the workflow. Use `aube import` only if the team intentionally wants
to convert the project onto `aube-lock.yaml`; after import succeeds, remove
`pnpm-lock.yaml` so future installs keep writing the aube lockfile.
