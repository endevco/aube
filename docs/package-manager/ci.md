---
description: Use committed lockfiles, choose cache directories, and prepare production installs with aube in CI and containers.
---

# CI and containers

Commit the project's lockfile and build approvals before adding aube to CI.
A frozen install fails when the manifest and lockfile disagree, making the
failure visible instead of updating dependency versions during a build.

## Choose an install command

| Command | Existing `node_modules` | Lockfile behavior |
| --- | --- | --- |
| `aube ci` | Removed before installation | Requires a fresh committed lockfile |
| `aube install --frozen-lockfile` | Can reuse the current install | Requires a fresh committed lockfile |
| `aube install --prod --frozen-lockfile` | Installs production dependencies | Requires a fresh committed lockfile |

Use `aube ci` for a clean build. Use `--frozen-lockfile` when retaining an
existing install is useful. Set the flag explicitly in scripts so the intent
is clear outside CI too.

## GitHub Actions

The [aube setup action](https://github.com/jdx/aube-action) installs the native
binary and can install Node.js in the same step:

```yaml
name: Test
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: jdx/aube-action@v1
        with:
          node-version: "24"
      - run: aube ci
      - run: aube run --no-install test
```

Use the Node version required by your project. The final command skips the
auto-install check because the preceding step already installed dependencies.
See the action's README for version pins, inputs, and outputs.

## Dependency builds

CI cannot make an interactive build-approval decision. Review required scripts
locally with `aube ignored-builds` and `aube approve-builds`, then commit the
resulting workspace YAML. `strictDepBuilds: true` makes unreviewed dependency
builds fail installation instead of being skipped.

For stricter policy, see the [security settings](/security#the-paranoid-switch).
If you enable jailed builds, check the runner's
[platform requirements](/package-manager/jailed-builds#native-enforcement).

## Cache choices

`aube store path` prints the resolved content store, including its `v1/`
directory. Registry metadata lives separately under the configured cache
directory. `aube doctor` shows the resolved paths.

The [global virtual store](/package-manager/global-virtual-store) is disabled
in CI by default. Cache package files when useful; a frozen lockfile still
controls which versions are installed. A cache is an optimization, not a
replacement for the committed lockfile or build policy.

## Container builds

Install aube in the image using one of the [installation methods](/installation),
then copy dependency inputs before application source when arranging cacheable
layers. Include the lockfile, `package.json`, workspace manifests, patches, and
configuration that affect resolution.

```sh
# Build stage: include development tools.
aube install --frozen-lockfile
aube run --no-install build

# Runtime stage: install only runtime dependencies.
aube install --prod --frozen-lockfile
```

Run the commands in their respective stages; they are not a complete Dockerfile.
Root lifecycle scripts may require source files during installation. Copy those
files before the install, or explicitly defer scripts if the project supports it.

For a workspace package, [deploy](/cli/deploy) can prepare a target directory
with publishable files and installed dependencies:

```sh
aube --filter @acme/api deploy dist/api
```

## When CI rejects the lockfile

Reproduce the failure locally with `aube install --frozen-lockfile`. If the
manifest change was intentional, run `aube install`, review the resulting diff,
and commit the updated lockfile. See [troubleshooting](/troubleshooting#the-lockfile-is-out-of-sync)
for repair options.
