---
description: Choose lockfile, offline, production, linker, and file-import modes for aube install.
---

# Install dependencies

`aube install` installs the dependencies declared in `package.json` and the
workspace manifests.

```sh
aube install
```

Most local work does not need a separate install command. `aubr <script>`,
`aube test`, and `aube exec <bin>` check install freshness first. If
`package.json` or the lockfile changed, aube installs before running the
script or binary. For one-off tools, `aubx <pkg>` uses a matching local binary or installs into
a throwaway environment.

Use `aube install` when the install itself is the task: first local setup
without running a script, lockfile updates, Docker layers, production-only
installs, offline installs, linker experiments, and CI flows.

## Lockfile modes

| Mode | Command | Use it when |
| --- | --- | --- |
| Prefer frozen | `aube install --prefer-frozen-lockfile` | Local default: reuse a fresh lockfile, re-resolve on drift. |
| Frozen | `aube install --frozen-lockfile` | CI should fail if `package.json` and lockfile disagree. |
| No frozen | `aube install --no-frozen-lockfile` | You want a full re-resolve. |
| Fix lockfile | `aube install --fix-lockfile` | You want to repair only entries that drifted. |
| Lockfile only | `aube install --lockfile-only` | You want to update the lockfile without linking `node_modules`. |

`aube ci` is the strict CI shortcut: it deletes `node_modules` and then runs a
frozen install.

## Dependency filters

```sh
aube install --prod
aube install --no-optional
```

`--prod` skips `devDependencies`. `--no-optional` skips optional dependencies.

## Network modes

```sh
aube install --prefer-offline
aube install --offline
```

`--prefer-offline` uses cached metadata when available and only hits the
network on a miss. `--offline` forbids network access entirely.

## Linker modes

```sh
aube install --node-linker=isolated
aube install --node-linker=hoisted
```

`isolated` is the pnpm-compatible default. It writes a strict symlink tree under
`node_modules/.aube/`. `hoisted` writes a flatter npm-style tree for projects
that need legacy `node_modules` assumptions. `pnp` is not supported.

## Store import methods

```sh
aube install --package-import-method=auto
aube install --package-import-method=hardlink
aube install --package-import-method=copy
aube install --package-import-method=clone-or-copy
```

`auto` chooses a filesystem strategy: generally reflinks on macOS and
hardlinks on Linux, with copies when linking is unavailable. Small files may
be copied directly. See [`packageImportMethod`](/settings/#setting-packageimportmethod)
for platform details and explicit clone/copy modes.

## Next steps

- [CI and containers](/package-manager/ci): choose frozen installs and caches.
- [Lifecycle scripts](/package-manager/lifecycle-scripts): review packages that need to build.
- [`aube install` reference](/cli/install): all arguments and flags.
