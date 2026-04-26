# Jailed dependency builds

Dependency lifecycle scripts are one of the sharpest supply-chain edges in a
JavaScript install. aube already keeps dependency scripts skipped until a
project approves them with `allowBuilds` / `onlyBuiltDependencies`. Jailed
builds would add a second boundary: approved packages may build, but they do
not automatically get the user's full filesystem, network, and environment.

Jailed builds are opt-in today. Enable them in workspace config:

```yaml
jailBuilds: true
```

If one reviewed package cannot run in the jail yet, keep jailed builds enabled
globally and exempt only that package:

```yaml
jailBuilds: true
neverJailBuiltDependencies:
  - sharp
```

## Goals

- Keep dependency lifecycle scripts denied by default.
- Run approved dependency scripts inside a narrow build jail.
- Prevent approved build scripts from reading credentials or mutating unrelated
  project and user files.
- Preserve compatibility for common native-package builds such as `esbuild`,
  `sharp`, `node-gyp`, `prebuild-install`, and `napi-postinstall`.
- Avoid Docker, daemon processes, images, and other heavyweight runtime
  dependencies.

## Default profile

When `jailBuilds` is enabled and a dependency is approved through `allowBuilds` or
`onlyBuiltDependencies`, aube can run its `preinstall`, `install`, and
`postinstall` scripts with a default native jail profile:

| Capability | Default |
| --- | --- |
| Filesystem reads | unrestricted today; package/toolchain-only reads are planned |
| Filesystem writes | package directory and aube-owned temporary directories |
| Network | denied |
| Environment | scrubbed allowlist only |
| Home directory | temporary aube-owned jail home |

The important distinction is that approval means "this package may build
itself." It does not mean "this package may write shell startup files, modify
unrelated workspace files, inherit registry tokens, or reach the network."

## Package permissions

The package-specific permissions shape below is the planned extension for
packages that genuinely need extra access:

```yaml
buildPermissions:
  esbuild:
    jail: native
    read:
      - package
      - deps
      - toolchain
    write:
      - package
      - tmp
    net: false
    env:
      - PATH
      - HOME
      - npm_*
      - npm_config_arch
      - npm_config_platform

  sharp:
    jail: native
    read:
      - package
      - deps
      - toolchain
    write:
      - package
      - tmp
    net:
      - registry.npmjs.org
      - github.com
    env:
      - PATH
      - HOME
      - npm_config_*
```

Boolean `allowBuilds` entries stay compatible with pnpm and continue to mean
"approved to run." aube-specific `buildPermissions` narrow or widen the
jail used after that approval decision.

Today, `neverJailBuiltDependencies` is the package-level escape hatch. Entries
use the same package-pattern syntax as `neverBuiltDependencies`, and only
disable the jail; they do not bypass the build approval policy.

## Native enforcement

The jail uses the same lightweight strategy as mise on macOS:

- macOS: generate a Seatbelt profile and run scripts through `sandbox-exec` to
  deny network access and writes outside the package / temporary directories.
- Linux: environment and HOME isolation are enabled today. Landlock and seccomp
  native enforcement are planned.
- Windows: start with environment scrubbing, a temporary home directory, and an
  unsupported-native-jail warning until there is a good OS-native policy.

The implementation should live below the script runner rather than the install
driver. Every npm-style lifecycle path funnels through
`aube_scripts::run_script`, so the install path, `rebuild`, and other callers
can share one enforcement point.

## Quarantined build directory

The stronger future mode is to build each dependency in quarantine:

1. Reflink, hardlink, or copy the package into an aube-owned temporary build
   directory.
2. Run lifecycle scripts with writes limited to that build directory and a
   temporary jail home.
3. Copy the resulting package tree back into the linked package directory after
   a successful build.
4. Save that result in the side-effects cache when caching is enabled.

This keeps build output package-local and prevents a script from mutating
sibling packages, project files, lockfiles, global stores, or unrelated
`node_modules` state.

## Environment policy

Dependency scripts should receive only the environment they need to behave like
npm lifecycle scripts:

- `PATH`
- `HOME`, pointing at the jail home
- `INIT_CWD`
- `npm_lifecycle_event`
- `npm_package_name`
- `npm_package_version`
- selected `npm_config_*` values needed for platform and build tooling

Tokens are denied unless a future package-specific permission allows them:

- `AUBE_AUTH_TOKEN`
- `NPM_TOKEN`
- `NODE_AUTH_TOKEN`
- `GITHUB_TOKEN`
- `SSH_AUTH_SOCK`
- `AWS_*`
- `GOOGLE_*`
- `AZURE_*`

Root lifecycle scripts can remain unjailed at first because they are project
code. The supply-chain boundary is dependency code.

## Rollout

1. Add `jailBuilds` as an opt-in for dependency lifecycle scripts.
2. Add package/toolchain-only read enforcement.
3. Add Linux Landlock / seccomp enforcement.
4. Teach `aube approve-builds` to show the default jail profile for newly
   approved packages.
5. Add `buildPermissions` to `aube-workspace.yaml`.
6. Make jailed dependency builds the default on supported platforms.
7. Keep explicit config escape hatches for debugging:
   `jailBuilds=false` globally, or `neverJailBuiltDependencies` for a package.

The escape hatch should be noisy in CI-oriented output because disabling the
jail turns an approved dependency build back into ambient code execution.
