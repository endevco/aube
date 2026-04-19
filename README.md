<p align="center">
  <a href="https://aube.en.dev">
    <img src="assets/logo.svg" alt="aube logo" width="140" height="140">
  </a>
</p>

<h1 align="center">aube</h1>

<p align="center">
  A fast Node.js package manager that drops into existing projects.
</p>

<p align="center">
  <strong>aube</strong> means dawn in French. Pronounced <code>/ob/</code>, like "ohb".
</p>

<p align="center">
  <strong><a href="https://aube.en.dev">Read the docs</a></strong>
</p>

> [!WARNING]
> aube is beta software. It aims for compatibility with pnpm v11, but it has not been tested across many projects yet.

## Why Try It

**[Fast installs](https://aube.en.dev/benchmarks).** Warm CI is about 7x faster than pnpm and 3x faster than Bun in the current benchmarks. Across the fixture set, aube runs roughly 1-21x faster than pnpm and up to 3x faster than Bun.

**[Existing lockfiles](https://aube.en.dev/package-manager/lockfiles).** Reads and writes `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock` in place.

**[Cheap repeat commands](https://aube.en.dev/package-manager/scripts).** `aube run test`, `aube test`, and `aube exec vitest` auto-install when dependencies are stale, then skip that work when nothing changed.

**[Less disk use](https://aube.en.dev/package-manager/node-modules).** A global content-addressable store lets projects share package files instead of keeping a full copy of the same dependencies in every checkout.

**[Secure defaults](https://aube.en.dev/package-manager/configuration).** aube defaults to safer installs: new releases wait out a minimum age, exotic transitive dependencies are blocked, and dependency lifecycle scripts require approval.

## Install

The recommended path is mise:

```sh
mise use -g aube
```

Check that it is on your `PATH`:

```sh
aube --version
```

Inside a project, you can also pin aube with mise:

```sh
mise use aube
```

aube is also published on npm:

```sh
npm install -g @endevco/aube
```

While aube is beta, Homebrew installs come from the Endev tap:

```sh
brew install endevco/tap/aube
```

See [other install methods](https://aube.en.dev/installation).

## First Install

Run aube in an existing Node.js project:

```sh
aube install
```

If the project already has a supported lockfile, aube reads it and writes updates back to the same file. That makes it easy to try aube locally without forcing the rest of the team to switch package managers first.

For a new project with no lockfile, aube creates `aube-lock.yaml`.

## Daily Commands

```sh
aube install              # install dependencies
aube add react            # add a dependency
aube add -D vitest        # add a dev dependency
aube remove react         # remove a dependency
aube update               # update dependencies within package.json ranges
aube run build            # run a package.json script
aube test                 # run the test script, auto-installing first if needed
aube exec vitest          # run a local binary
aube dlx cowsay hi        # run a package in a throwaway environment
aube ci                   # clean, frozen install for CI
```

You can also run scripts directly:

```sh
aube dev
aube build
aube lint
```

If the script exists in `package.json`, aube treats that as `aube run <script>`.

### Shortcuts: `aubr` and `aubx`

`aubr` and `aubx` are multicall shims for `aube run` and `aube dlx`. They
share a binary with `aube` and dispatch purely on `argv[0]`, so every flag
that works on the full command also works on the shim:

```sh
aubr build            # aube run build
aubx cowsay hi        # aube dlx cowsay hi
```

The release archives ship all three binaries side by side; no extra
setup is needed when you install aube via mise or the tarball.

## CI

Use `aube ci` when the lockfile must be treated as the source of truth:

```sh
aube ci
```

It removes `node_modules`, verifies the lockfile is fresh for the current `package.json`, then installs.

For Docker layers or workflows where you only want to update the lockfile:

```sh
aube install --lockfile-only
```

For production-only installs:

```sh
aube install --prod
```

## Workspaces

aube supports workspace projects and the `workspace:` protocol.

```sh
aube install -r
aube run test -r
aube add zod --filter @acme/api
```

If a project already uses `pnpm-workspace.yaml`, aube can read and write it. New aube-first workspaces can use `aube-workspace.yaml`.

## Lockfile Compatibility

| File | Reads | Writes in place |
| --- | --- | --- |
| `aube-lock.yaml` | yes | yes |
| `pnpm-lock.yaml` v9 | yes | yes |
| `package-lock.json` v2/v3 | yes | yes |
| `npm-shrinkwrap.json` | yes | yes |
| `yarn.lock` (v1 classic + v2+ berry) | yes | yes |
| `bun.lock` | yes | yes |

aube is not compatible with every historical lockfile shape. Older pnpm v5/v6 lockfiles should be upgraded with pnpm before switching. Yarn PnP projects need to move to a `node_modules` linker first — aube writes `node_modules`, not `.pnp.cjs`.

When more than one lockfile exists, prefer keeping one canonical lockfile for the project so teammates and CI do not fight over dependency state.

## Dependency Scripts

aube skips dependency lifecycle scripts by default. That protects installs from unexpected build steps in transitive packages.

To allow packages that need build scripts:

```sh
aube approve-builds
```

You can inspect packages whose scripts were skipped:

```sh
aube ignored-builds
```

## Package Layout

aube uses an isolated `node_modules` layout. Packages are linked through `node_modules/.aube/`, and package files are stored once in `~/.aube-store/`.

That means:

- several projects with similar dependencies share package files and use less disk space;
- dependencies stay isolated, so phantom dependencies are harder to rely on accidentally;
- repeated installs can reuse package files already on disk.

## Commands You May Recognize

aube supports the common package-manager surface:

```sh
aube list
aube why react
aube outdated
aube audit
aube pack
aube publish
aube link
aube unlink
aube config get registry
aube store path
aube store prune
```

Some pnpm commands are intentionally out of scope. Runtime-management commands such as `env`, `runtime`, `setup`, and `self-update` belong in tools like mise. Registry account helpers such as `whoami`, `token`, `owner`, `search`, `pkg`, and `set-script` are compatibility stubs that point you to the npm command instead.

## Learn More

- [Documentation](https://aube.en.dev)
- [Benchmarks](https://aube.en.dev/benchmarks)
- [Lockfile compatibility](https://aube.en.dev/package-manager/lockfiles)
- [Run scripts and binaries](https://aube.en.dev/package-manager/scripts)

## CI

<p>
  <a href="https://buildkite.com">
    <img src="assets/buildkite-logo.svg" alt="Buildkite" width="180">
  </a>
</p>

Thanks to [Buildkite](https://buildkite.com) for providing CI for aube.

## Contributors

[![Contributors](https://contrib.rocks/image?repo=endevco/aube)](https://github.com/endevco/aube/graphs/contributors)

<p>
  <a href="https://en.dev">
    <img src="https://github.com/endevco.png?size=96" alt="en.dev" width="42" height="42" align="left">
  </a>
  Built by <a href="https://en.dev">en.dev</a>.
</p>

<br clear="left">

## License

MIT
