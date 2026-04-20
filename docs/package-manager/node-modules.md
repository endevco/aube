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

## Global virtual store

aube has two "stores":

- The **global content store** (`$XDG_DATA_HOME/aube/store/v1/files/`) holds
  package *files* deduplicated by BLAKE3 hash. Every install reads from it.
- The **global virtual store** (`~/.cache/aube/virtual-store/`) holds
  materialized package *directories* keyed by dependency-graph hash.
  `node_modules/.aube/<pkg>` is an absolute symlink into this shared location,
  so repeat installs across projects reuse the same tree instead of
  re-materializing it.

The global virtual store is on by default outside CI and off under CI (where a
warm cache is rarely available). Override per-project with
`enableGlobalVirtualStore=true|false` in `.npmrc`, or per-invocation with
`--enable-global-virtual-store` / `--disable-global-virtual-store`.

### Why aube turns it off for some packages

A few tools resolve modules by canonicalizing every `node_modules/<pkg>`
symlink to its real path, then walking up the directory tree looking for
configs, app-router roots, or hoisted deps. When `<pkg>` points into the
global virtual store, the walk escapes the project and fails with errors
like `Symlink ... is invalid, it points out of the filesystem root`.

When any importer depends on a known-incompatible package, aube falls back
to per-project materialization under `node_modules/.aube/` and prints a
one-line warning:

```text
`next` isn't compatible with aube's global virtual store — installing
per-project instead. Install still succeeds; repeat installs of this
project just won't share materialized packages across projects.
```

- **How to fix it properly.** The package itself would need to stop
  canonicalizing through symlinks (pnpm users hit the same class of issue).
  That's an upstream change in the tool — please file it with the package
  maintainers, not with aube.
- **How to silence the warning.** Add `enableGlobalVirtualStore=false` to
  `.npmrc`. The per-project fallback stays on; the warning just goes away.
- **How to opt out of the heuristic entirely** (at your own risk — expect
  the errors above): set `disableGlobalVirtualStoreForPackages=[]` in
  `.npmrc`.

The default trigger list is `next`, `nuxt`, `vite`, `vitepress`, `parcel` —
the tools with concrete, reproducible walk-up failures. Add names to
`disableGlobalVirtualStoreForPackages` if you hit the same class of error
with another package.

## Coexistence with pnpm

aube does not reuse `node_modules/.pnpm/` or `~/.pnpm-store/`. If a pnpm-built
tree already exists, aube installs alongside it in `node_modules/.aube/`.

