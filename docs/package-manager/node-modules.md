---
description: Understand isolated and hoisted dependency layouts, shared package files, and virtual stores.
---

# node_modules layout

aube defaults to isolated dependencies: a package resolves the dependencies
it declares through symlinks beside its installed files. Compatible local
projects use a shared virtual store; CI uses a project-local store by default.

The project-local layout looks like this:

```text
project/
  node_modules/
    react -> .aube/react@18.2.0/node_modules/react
    .aube/
      react@18.2.0/
        node_modules/
          react/
          loose-envify -> ../../loose-envify@1.4.0/node_modules/loose-envify
```

## Why isolated

Only declared direct dependencies appear at the project top level. Transitive
dependencies are linked next to the packages that declared them, so phantom
dependencies fail instead of being accidentally available.

## Hoisted mode

```sh
aube install --node-linker=hoisted
```

Hoisted mode writes a flatter npm-style tree for tools that assume most
packages are visible at the top level.

## Global store

Package files are stored by content hash under:

```text
$XDG_DATA_HOME/aube/store/v1/
```

This defaults to `~/.local/share/aube/store/v1/` when
`$XDG_DATA_HOME` is unset. Run `aube store path` to see the resolved
location.

aube imports files from that store into the virtual store with reflinks,
hardlinks, or copies depending on filesystem support and
`package-import-method`.

## Global virtual store

The [global virtual store](/package-manager/global-virtual-store) reuses
materialized package directories across projects. It is on by default outside
CI and off under CI.

## Coexistence with pnpm

aube uses its own stores and does not reuse pnpm's `.pnpm/` virtual store.
Both virtual-store directories can exist, but the package manager that installs
last controls the project's top-level dependency links. Run that manager's
install command after switching tools.
