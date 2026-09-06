<p align="center">
  <a href="https://aube.sh"><img src="assets/logo.svg" alt="aube" width="120" height="120"></a>
</p>

<h1 align="center">aube</h1>

<p align="center"><strong>Run your project. Dependencies take care of themselves.</strong></p>

<p align="center">
  A Node.js package manager written in Rust. Installs automatically before running
  scripts, shares packages across projects, and updates your existing lockfile in place.
</p>

<p align="center">
  <a href="https://aube.sh/getting-started">Get started</a> ·
  <a href="https://aube.sh/guide">Documentation</a> ·
  <a href="https://aube.sh/cli/">Commands</a> ·
  <a href="https://aube.sh/benchmarks">Benchmarks</a>
</p>

## Get started

Install with [mise](https://mise.jdx.dev), then run a script in your project:

```sh
mise use -g aube
aube --version
cd your-project
aubr build
```

Use a script defined in your `package.json`. `aubr` is shorthand for `aube run`:
when dependencies are missing or stale, it installs them before starting the
script. Repeat runs skip the install when nothing has changed.

Prefer another installer? See [Homebrew, npm, Cargo, Linux packages, and source builds](https://aube.sh/installation).
To pin aube for a project with mise, run `mise use aube` inside that project.

## Why aube

<!-- BENCH_RATIOS:START -->
**[Fast installs](https://aube.sh/benchmarks).** Warm installs are about 8x faster than pnpm and about 3x faster than Bun in the current benchmarks. Repeat test commands run up to 24x faster than pnpm and up to 2x faster than Bun.
<!-- BENCH_RATIOS:END -->

Those results describe the recorded fixtures and cache conditions. See the
[methodology and all scenarios](https://aube.sh/benchmarks) for the comparison.

- **Keep your lockfile.** Reads and writes pnpm, npm, Yarn, and Bun text lockfiles
  in place. New projects default to `aube-lock.yaml`.
- **Install as part of the work.** `aubr build`, `aube test`, and `aube exec vitest`
  check dependency freshness before running. `aubx` runs one-off tools.
- **Share package files.** A content-addressable store deduplicates files;
  the global virtual store also reuses package directory trees across local projects.
- **Review dependency code.** Dependency build scripts need an allow rule or
  built-in trust. Explicit denies take precedence. Optional build jails restrict
  approved scripts; release-age and publishing-trust checks run during resolution.

## Everyday commands

| Task | Command |
| --- | --- |
| Run a project script | `aubr build` |
| Run the test script | `aube test` |
| Run a local binary | `aube exec vitest` |
| Run a one-off tool | `aubx cowsay hi` |
| Add a dependency | `aube add react` |
| Add a development dependency | `aube add -D vitest` |
| Remove a dependency | `aube remove react` |
| Update within manifest ranges | `aube update` |
| Install without running a project script | `aube install` |
| Clean install from a committed lockfile | `aube ci` |

`aubr <name>` prefers a package script, then a local binary. `aubx <name>`
prefers a local binary, then installs the tool in a throwaway project. Use
`aubx --package <package> <binary>` to request a separate tool installation.
See [scripts and binaries](https://aube.sh/package-manager/scripts) for flags,
argument forwarding, and workspace runs.

## Try it in an existing project

| Existing lockfile | Supported format |
| --- | --- |
| `pnpm-lock.yaml` | Lockfile v9, written by pnpm 9–11 |
| `package-lock.json` | v2 and v3 |
| `npm-shrinkwrap.json` | npm shrinkwrap |
| `yarn.lock` | Classic v1 and Berry v2+ |
| `bun.lock` | Text format v1 |

Run `aube install`, inspect the diff, and run your tests. You do not need to
import or delete a supported lockfile. Upgrade older pnpm lockfiles with pnpm
first; convert `bun.lockb` with Bun. Yarn PnP projects need a `node_modules`
linker. Keeping the lockfile format does not guarantee identical behavior:
aube uses isolated dependencies, its own stores, and its own security defaults.

Migration guides: [pnpm](https://aube.sh/pnpm-users) ·
[npm](https://aube.sh/npm-users) · [Yarn](https://aube.sh/yarn-users) ·
[Bun](https://aube.sh/bun-users).

## Dependency builds and security

Root lifecycle scripts run during install unless `--ignore-scripts` is set.
Dependency scripts run only when allowed by project policy or aube's built-in
trusted-dependencies list. Review skipped builds with:

```sh
aube ignored-builds
aube approve-builds
aube rebuild
```

Commit the resulting `allowBuilds` policy so teammates and CI use the same
approvals. To restrict approved dependency builds, set `jailBuilds: true` in
`aube-workspace.yaml` or an existing `pnpm-workspace.yaml`.

The jail's filesystem and network enforcement depends on the OS; filesystem
reads are currently unrestricted. See [security defaults](https://aube.sh/security)
and [jailed builds](https://aube.sh/package-manager/jailed-builds) for the exact
boundaries and the optional `paranoid` bundle.

## Workspaces and Node.js

```sh
aube -r run test
aube --filter @acme/api add zod
aube runtime set node 24 --save-exact
```

aube reads an existing `pnpm-workspace.yaml` in place. New workspaces can use
`aube-workspace.yaml`. Both support workspace packages, filters, and catalogs.

Commands run through aube use the project's Node pin from `devEngines.runtime`,
`.node-version`, or `.nvmrc`. Optional shell activation also routes ordinary
`node`, `npm`, `pnpm`, and `yarn` commands through aube.
See [workspaces](https://aube.sh/package-manager/workspaces) and
[Node runtime switching](https://aube.sh/package-manager/node-runtime).

## Find your next step

- [CI and containers](https://aube.sh/package-manager/ci): frozen installs, production dependencies, and cache choices.
- [Configuration](https://aube.sh/package-manager/configuration): project and user settings, registries, and policy.
- [Troubleshooting](https://aube.sh/troubleshooting): diagnose installs, scripts, and tool compatibility.
- [Embedding](https://aube.sh/embedding/): use aube from Rust, Node-API, or a C ABI host.
- [Contributing](CONTRIBUTING.md): build, test, and improve aube.

Questions and bug reports belong in [GitHub Discussions](https://github.com/jdx/aube/discussions).
Report vulnerabilities through the [security policy](SECURITY.md).

*aube* means dawn in French, pronounced `/ob/` ("ohb"). Built by [jdx](https://jdx.dev).

## Sponsors

<p align="center">
  Sponsored by<br><br>
  <a href="https://entire.io">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://jdx.dev/sponsors/entire-lockup.svg">
      <img src="https://jdx.dev/sponsors/entire-lockup-on-light.svg" alt="Entire" height="36">
    </picture>
  </a>
  &nbsp;&nbsp;&nbsp;
  <a href="https://omarchy.org/patrons/">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://jdx.dev/sponsors/omacom-foundation.svg">
      <img src="https://jdx.dev/sponsors/omacom-foundation-on-light.svg" alt="Omacom Foundation" height="36">
    </picture>
  </a>
  <br><br>
  <a href="https://jdx.dev/sponsors.html">View all sponsors</a>
</p>

## Star History

<a href="https://www.star-history.com/?repos=jdx%2Faube&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=aubepkg/aube&type=date&theme=dark&legend=top-left&sealed_token=DittFna0Wy8uarlDVdMQmftPSDrk5iHeM_2M3W29oCIHgkBqNaXeWjnIPLZF985B9f4lmEGXoWpmfS7b4XyGnSJG9N3wY4gIl2jcX_F7ubwVv-aO9YDgVa76qt3ec9ObE2jVxQW9PklITRQf2Q_DCdKU5ZN5Dr489tSkLetEDtcrRIryb1NQH7xM_e3U" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=aubepkg/aube&type=date&legend=top-left&sealed_token=DittFna0Wy8uarlDVdMQmftPSDrk5iHeM_2M3W29oCIHgkBqNaXeWjnIPLZF985B9f4lmEGXoWpmfS7b4XyGnSJG9N3wY4gIl2jcX_F7ubwVv-aO9YDgVa76qt3ec9ObE2jVxQW9PklITRQf2Q_DCdKU5ZN5Dr489tSkLetEDtcrRIryb1NQH7xM_e3U" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=aubepkg/aube&type=date&legend=top-left&sealed_token=DittFna0Wy8uarlDVdMQmftPSDrk5iHeM_2M3W29oCIHgkBqNaXeWjnIPLZF985B9f4lmEGXoWpmfS7b4XyGnSJG9N3wY4gIl2jcX_F7ubwVv-aO9YDgVa76qt3ec9ObE2jVxQW9PklITRQf2Q_DCdKU5ZN5Dr489tSkLetEDtcrRIryb1NQH7xM_e3U" />
 </picture>
</a>

## Contributors

[![Contributors](https://contrib.rocks/image?repo=aubepkg/aube)](https://github.com/aubepkg/aube/graphs/contributors)

<p>
  <a href="https://jdx.dev">
    <img src="https://github.com/jdx.png?size=96" alt="jdx" width="42" height="42" align="left">
  </a>
  Built by <a href="https://jdx.dev">jdx</a>.
</p>

<br clear="left">

## License

[MIT](LICENSE)
