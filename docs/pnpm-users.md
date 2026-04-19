# For pnpm users

aube should be a drop-in replacement for pnpm projects. There are only
minor differences in behavior.

## Behavior differences

A handful of commands behave differently in a way that's worth knowing
before you ship an aube-based workflow:

| Command | Difference |
| --- | --- |
| `aube run <script>` | Checks install staleness and **auto-installs** before running. `pnpm run` does not. |
| `aube test` | **Auto-installs** first, then runs the `test` script — equivalent to `pnpm install-test` in one command. |
| `aube exec <bin>` | **Auto-installs** on stale state before running. `pnpm exec` does not install. |
| `aube install` (new project) | Creates `aube-lock.yaml` if there's no existing lockfile. pnpm creates `pnpm-lock.yaml`. In an existing pnpm project, aube reads and writes `pnpm-lock.yaml` in place. |

Everything else — `add`, `remove`, `update`, `dlx`, `list`, `why`, `pack`,
`publish`, `approve-builds` — matches pnpm's behavior.

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
| Install state | `node_modules/.modules.yaml` | `node_modules/.aube-state` |
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

- **Separate install locations.** Installs go into `node_modules/.aube/` and
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
- **Speed.** See the [benchmarks](/benchmarks).

## Supported pnpm lockfile versions

aube reads and writes `pnpm-lock.yaml` at **lockfile version 9** — the
format shipped by pnpm v9 and later. Older pnpm lockfiles (versions 5, 6,
7, and 8, used by pnpm 7.x and 8.x) are not supported and will cause aube
to refuse the install.

To upgrade an older pnpm lockfile, run a modern pnpm once to convert it:

```sh
npx pnpm@latest install
```

That rewrites `pnpm-lock.yaml` at v9. Commit the result, then switch to
`aube install`.

## Out of scope

aube does not manage Node.js itself. Runtime-management commands like
`pnpm env`, `pnpm runtime`, `pnpm setup`, and `pnpm self-update` are
intentionally not implemented — use [mise](https://mise.jdx.dev) to
install and switch Node versions:

```sh
mise use node@22
```
