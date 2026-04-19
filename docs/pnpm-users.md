# For pnpm users

aube should be a drop-in replacement for pnpm projects. There are only
minor differences in behavior.

## Command map

| pnpm | aube | Notes |
| --- | --- | --- |
| `pnpm install` | `aube install` | Reads and updates an existing `pnpm-lock.yaml` in place. Only new projects (no supported lockfile on disk yet) default to `aube-lock.yaml`. |
| `pnpm add react` | `aube add react` | Supports dependency sections, exact pins, peer deps, workspace root adds, and globals. |
| `pnpm remove react` | `aube remove react` | Removes from the manifest and relinks. |
| `pnpm update` | `aube update` | Updates all or named direct dependencies. |
| `pnpm run build` | `aube run build` | Runs scripts with an auto-install staleness check first. |
| `pnpm test` | `aube test` | Shortcut for the `test` script; aube auto-installs first (equivalent to `pnpm install-test`). |
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
| Install state | pnpm-owned metadata | `.aube/.state/install-state.json` |
| Workspace manifest | `pnpm-workspace.yaml` | `aube-workspace.yaml` |

aube reads pnpm v11 YAML files for compatibility. `aube-lock.yaml` and
`aube-workspace.yaml` use pnpm-compatible shapes today but are the long-term
contract and may diverge after the beta compatibility push.

aube never touches pnpm's `node_modules/.pnpm/` or `~/.pnpm-store/`. The two
virtual stores can coexist under `node_modules`. For the lockfile and
workspace YAML, aube reads and writes whichever file already exists on disk
— `pnpm-lock.yaml` keeps getting updates in place, and `pnpm-workspace.yaml`
is read in place (aube does not emit an `aube-workspace.yaml` for existing
projects).

## What's different

- **aube-owned directories.** Installs go into `node_modules/.aube/` and
  `~/.aube-store/` instead of pnpm's `.pnpm/` and `~/.pnpm-store/`. If a
  project already has a pnpm-built `node_modules`, aube installs alongside
  — the two virtual stores live side by side.
- **Default YAML filenames for new projects.** A project with no lockfile
  yet gets `aube-lock.yaml`. If it already has `pnpm-lock.yaml` (or any
  other supported lockfile — `package-lock.json`, `npm-shrinkwrap.json`,
  `yarn.lock`, `bun.lock`), aube reads and writes that file in place.
  Install / add / remove / update never touch workspace YAML. The one
  exception is `aube approve-builds`, which writes approvals into
  `pnpm-workspace.yaml`'s `onlyBuiltDependencies` (matching pnpm v10+),
  creating the file if missing. aube does not generate an
  `aube-workspace.yaml` for you — create it yourself if you want the
  aube-named variant.
- **Build approvals.** Dependency lifecycle script approval follows pnpm
  v11's allowlist model. Use explicit policy fields in `package.json` or
  `aube-workspace.yaml` to opt in.
- **`aube test` auto-installs.** Equivalent to `pnpm install-test`: aube
  auto-installs before running `test`, so the two-step pnpm workflow
  becomes one command. `aube run` and `aube exec` do the same staleness
  check.
- **Speed.** See the [benchmarks](/benchmarks).

## Out of scope

Runtime-management commands like `pnpm env`, `pnpm runtime`, `pnpm setup`,
and `pnpm self-update` are intentionally not implemented. For a compact
gap list, see the
[README compatibility notes](https://github.com/endevco/aube#commands-you-may-recognize).
For command and flag details, see the [CLI reference](/cli/).

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
