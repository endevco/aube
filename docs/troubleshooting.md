# Troubleshooting

## A package is missing from `node_modules`

Run:

```sh
aube install
aube list --depth Infinity <package>
```

In the isolated linker, only direct dependencies are symlinked at the top level.
Transitive dependencies live under the packages that declared them. If your app
imports a package directly, add it to your own `package.json`.

## A dependency build script did not run

Dependency lifecycle scripts follow the pnpm v11 build approval model. Inspect the
pending list:

```sh
aube ignored-builds
aube approve-builds
aube rebuild
```

Use `--dangerously-allow-all-builds` only for a local diagnostic run. Do not use
it as a permanent CI default.

## A lockfile format is unsupported

aube reads and writes the current supported lockfile formats listed on the
[lockfiles page](/package-manager/lockfiles). Older pnpm v5/v6 lockfiles
should be upgraded with pnpm first. Yarn PnP projects need to switch to
`nodeLinker: node-modules` before using aube — aube writes a regular
`node_modules` tree, not `.pnp.cjs`.

If a project has multiple lockfiles, keep one canonical lockfile before
rolling aube into CI.

## The lockfile is out of sync

For a strict CI install:

```sh
aube ci
```

For a local repair:

```sh
aube install --fix-lockfile
```

For a full re-resolve:

```sh
aube install --no-frozen-lockfile
```

## `aube run` installed unexpectedly

Script commands check install freshness first. If `package.json` or the
lockfile changed, aube installs before running the script:

```sh
aube run build
aube test
```

If you want to skip that check for one command:

```sh
aube run --no-install build
aube test --no-install
```

## The registry is unavailable

If the cache already contains the required metadata and tarballs:

```sh
aube install --offline
```

If you want to prefer cached metadata but allow network misses:

```sh
aube install --prefer-offline
```

## A private registry or scope is not authenticated

aube reads npm-compatible registry configuration from `.npmrc` and environment
variables.

```ini
@myorg:registry=https://registry.example.test
//registry.example.test/:_authToken=${NPM_TOKEN}
```

Check what aube sees:

```sh
aube config get registry
aube config list --json
```

For publishing and login flows, see [Registry and auth](/package-manager/registry-auth).

## A workspace filter matches nothing

Check the workspace package names and paths:

```sh
aube list -r --depth 0
```

Then retry with an exact name, glob, path selector, or graph selector:

```sh
aube -F '@scope/*' run build
aube -F './packages/api' test
aube -F 'api...' run build
```

## You need to fall back temporarily

Keep the existing lockfile while evaluating aube. Since aube writes supported
lockfiles in place, the original package manager can keep using the same file
during rollout. If aube hits a beta bug in a project, fall back for that job,
keep the failing command and lockfile handy, and open a thread in
[GitHub Discussions](https://github.com/endevco/aube/discussions) with the
exact command and package manager versions.
