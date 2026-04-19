# Getting Started

aube is a fast Node.js package manager that can run in existing projects
without changing the lockfile format first. If your project already has
`pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, or
`bun.lock`, aube reads it and writes updates back to the same file.

## Install

See the [installation guide](/installation).

## Use it

```sh
# install dependencies
aube install

# add a dependency
aube add lodash

# run a script from package.json
aube run build

# install + run the test script (equivalent to `pnpm install-test`)
aube test
```

aube checks install freshness before running scripts. If `package.json` or the
lockfile changed, `aube test`, `aube run build`, and `aube exec vitest`
install first; repeated runs skip that work.

## Learn the package-manager flow

- [For pnpm users](/pnpm-users) maps the common pnpm commands and files
  to aube.
- [Install dependencies](/package-manager/install) covers lockfile modes,
  production installs, offline installs, and linker modes.
- [Manage dependencies](/package-manager/dependencies) covers adding,
  removing, updating, deduping, and pruning dependencies.
- [Workspaces](/package-manager/workspaces) covers filters, recursive runs,
  catalogs, workspace dependencies, and deploys.
- [Lifecycle scripts](/package-manager/lifecycle-scripts) explains the
  dependency script allowlist model.
