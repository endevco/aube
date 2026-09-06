---
description: Define aube workspaces, link local packages, select projects with filters, and share versions with catalogs.
---

# Workspaces

Define workspace package paths in `aube-workspace.yaml` at the repository root.
If the project already has `pnpm-workspace.yaml`, keep it: aube reads and updates
that file in place. If both exist, `aube-workspace.yaml` takes precedence.

Each matched package needs its own `package.json` with a name. For example:

```yaml
packages:
  - "packages/*"
  - "apps/*"
```

Install from the workspace root, then run a script across packages:

```sh
aube install
aube -r run build
```

Recursive builds run in dependency order by default. Use the explicit
`workspace:` protocol when a dependency must resolve to a local package.

## Workspace protocol

```json
{
  "dependencies": {
    "@acme/ui": "workspace:*",
    "@acme/config": "workspace:^"
  }
}
```

Workspace dependencies are linked to local packages during development and
converted to concrete versions for publishing/deploy flows.

## Filters

```sh
aube -F api run build
aube -F '@acme/*' test
aube -F './apps/web' install
aube -F 'api...' run build
aube -F '...web' run test
aube -F '!legacy' -r run lint
```

Supported selector forms:

- Exact package names.
- Globs such as `@acme/*`.
- Paths such as `./apps/web`.
- Dependency graph selectors such as `api...` and `api^...`.
- Dependent graph selectors such as `...web` and `...^web`.
- Git-ref selectors such as `[origin/main]`.
- Exclusions such as `!legacy`.

## Recursive mode

```sh
aube -r run build
aube -r list --depth 0
```

`-r` runs over every workspace package unless an explicit filter is present.

## Catalogs

Catalogs keep shared version ranges in the workspace YAML. Define them in
`aube-workspace.yaml` or the existing `pnpm-workspace.yaml`:

```yaml
catalog:
  react: ^19.0.0
catalogs:
  test:
    vitest: ^3.0.0
```

```json
{
  "dependencies": {
    "react": "catalog:",
    "vitest": "catalog:test"
  }
}
```

## Deploy

```sh
aube -F api deploy dist/api
```

`deploy` copies the selected workspace package's publishable files, rewrites
workspace dependencies to concrete versions, and installs dependencies in the
target directory.
