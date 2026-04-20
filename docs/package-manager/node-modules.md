# node_modules layout

aube defaults to an isolated symlink layout like pnpm's `node-linker=isolated`.
The difference is directory ownership: aube writes `.aube/`, not `.pnpm/`.

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
$XDG_DATA_HOME/aube/store/v1/files/
```

This defaults to `~/.local/share/aube/store/v1/files/` when
`$XDG_DATA_HOME` is unset.

aube imports files from that store into the virtual store with reflinks,
hardlinks, or copies depending on filesystem support and
`package-import-method`.

## Coexistence with pnpm

aube does not reuse `node_modules/.pnpm/` or `~/.pnpm-store/`. If a pnpm-built
tree already exists, aube installs alongside it in `node_modules/.aube/`.

