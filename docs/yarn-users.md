---
description: Use Yarn Classic and Berry lockfiles with aube, prepare PnP projects, and review workspace and build-policy differences.
---

# For Yarn users

aube can install directly from both Yarn classic (v1) and Yarn berry (v2+)
lockfiles. You do not need to delete `yarn.lock` or remove `node_modules`
before trying aube.

## Yarn classic (v1)

```sh
aube install
```

Run this from the project root, then review the lockfile diff and run your
tests. For daily work, `aubr build`, `aube test`, and `aube exec <bin>`
install automatically when dependencies are stale. Use `aubx <pkg>` for
one-off tools.

aube reads and updates Yarn v1 `yarn.lock` in place and installs packages
into `node_modules/.aube/`.

Commit the updated `yarn.lock` so Yarn classic users and aube users see the
same resolved versions. You do not need `aube import` for a normal rollout;
`aube install` keeps `yarn.lock` as the shared source of truth.

Use `aube import` only if the team intentionally wants to convert the project
to `aube-lock.yaml`. The new `aube-lock.yaml` takes precedence on later installs. Retire the old
lockfile once the team has switched, to avoid maintaining two sources of truth.

## Yarn berry (v2+)

```sh
aube install
```

aube reads berry's YAML-format `yarn.lock` (the one with the
`__metadata:` header, `resolution:` / `checksum:` fields, and per-block
`linkType`) and writes the same format back. Berry's `checksum:`
values are preserved verbatim so `yarn install` against the rewritten
file still validates cached tarballs.

Supported protocols: `npm:` (the common case), `patch:` for local
patch files against npm-backed packages, `workspace:`, `file:`, `link:`,
`portal:`, `exec:`, plus `git:` / `git+ssh:` /
`git+https:` / `https:` URLs for remote sources. Entries that use
unsupported protocols are skipped with a warning — aube's dependency
graph doesn't model those yet, and they round-trip better through Yarn
itself.

## Yarn PnP

aube does not support Yarn's Plug'n'Play linker. Berry projects using
`nodeLinker: pnp` (the default) need to switch to `nodeLinker:
node-modules` before using aube as the install command:

```yaml
# .yarnrc.yml
nodeLinker: node-modules
```

Once the project writes a regular `node_modules` tree, `aube install`
can drop in against the same `yarn.lock`.

## Differences from Yarn

- aube keeps package files in a global content-addressable store.
- aube uses isolated symlinks instead of a hoisted flat tree by default.
- Workspace package discovery follows `aube-workspace.yaml` (or
  `pnpm-workspace.yaml` when the project already has one).
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`)
  run only when project policy or aube's built-in trusted-dependencies list
  allows them. Explicit denies win. Legacy `pnpm.onlyBuiltDependencies`
  entries are still honored. Approved dependency builds can also run in a
  [jail](/package-manager/jailed-builds) with package-specific env, path,
  and network permissions.

References:
[Yarn classic install](https://classic.yarnpkg.com/lang/en/docs/cli/install/)
·
[Yarn berry install](https://yarnpkg.com/cli/install)

## Next steps

See [lifecycle scripts](/package-manager/lifecycle-scripts) for build approvals,
[CI and containers](/package-manager/ci) for reproducible installs, and
[troubleshooting](/troubleshooting) for compatibility problems.
