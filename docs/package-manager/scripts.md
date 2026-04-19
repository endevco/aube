# Run scripts and binaries

aube follows npm and pnpm script conventions while adding an install-state
check before script execution.

## Scripts

```sh
aube run build
aube test
aube start
aube stop
aube restart
```

Before running a script, aube checks `node_modules/.aube-state`. If the
manifest or lockfile changed, aube installs first. Use `--no-install` when you
want to skip that check.

```sh
aube run --no-install build
aube test --no-install
```

Use `--if-present` for optional scripts:

```sh
aube run --if-present lint
```

## Local binaries

```sh
aube exec vitest
aube exec tsc -- --noEmit
```

`exec` runs a binary from the project context with `node_modules/.bin` on
`PATH`.

## One-off binaries

```sh
aube dlx cowsay hi
aube dlx -p create-vite create-vite my-app
```

`dlx` installs into a throwaway project and runs the requested binary.

## Shortcuts: `aubr` and `aubx`

`aubr` and `aubx` are multicall shims for `aube run` and `aube dlx`.
They ship side by side with `aube` in the release archives and dispatch
purely on `argv[0]`, so any flag that works on the full command works on
the shim:

```sh
aubr build            # aube run build
aubr -r test          # aube -r run test
aubx cowsay hi        # aube dlx cowsay hi
aubx -p create-vite create-vite my-app
```

The shims are identical aube binaries with a different filename; there is
nothing to configure. If you install aube by hand — for example by
copying the binary out of the tarball — bring `aubr` and `aubx` along so
the shortcuts resolve on `PATH`.

## Workspace runs

```sh
aube -r run build
aube -F '@scope/*' run test
aube -F './packages/api' exec tsc -- --noEmit
aube -F 'api...' run build
```

`-r` is sugar for `--filter=*`. Filters support exact names, globs, paths,
dependency/dependent graph selectors, git-ref selectors, and exclusions.
