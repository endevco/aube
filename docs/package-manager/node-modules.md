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

Package files and their cached indexes live side-by-side under the
store-version directory:

```text
$XDG_DATA_HOME/aube/store/v1/
├── files/   # content-addressed CAS shards (BLAKE3, 2-char sharding)
└── index/   # cached package indexes (name@version -> files map)
```

The `v1/` directory is what `aube store path` prints. The defaults
fall back to `~/.local/share/aube/store/v1/{files,index}/` when
`$XDG_DATA_HOME` is unset. Co-locating files and index means a single
backup, snapshot, or Docker BuildKit cache mount of that one path
captures the whole store — matching `pnpm store path` granularity.

aube imports files from that store into the virtual store with reflinks,
hardlinks, or copies depending on filesystem support and
`package-import-method`.

## Global virtual store

The [global virtual store](/package-manager/global-virtual-store) reuses
materialized package directories across projects. It is on by default outside
CI and off under CI.

## Coexistence with pnpm

aube does not reuse `node_modules/.pnpm/` or `~/.pnpm-store/`. If a pnpm-built
tree already exists, aube installs alongside it in `node_modules/.aube/`.
