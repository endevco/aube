# Contributing

Read the [contributing guide](docs/contributing.md) for setup, tests,
documentation, and pull-request expectations. The same guide is available
[on the docs site](https://aube.sh/contributing).

```sh
mise install
mise run build
mise run test
mise run docs:dev
```

## mbx build cache

`mise install` installs [mbx](https://mr-boxington.jdx.dev) 1.8. The normal
`mise run build`, `mise run test`, and `mise run lint` workflows activate its
transparent Cargo wrapper and therefore use the cache while invoking Cargo
normally. Standalone Cargo commands require an activated mise shell. To bypass
mbx without skipping or weakening a check, prefix the
equivalent Cargo command with `MBX_DISABLE=1`:

```sh
MBX_DISABLE=1 cargo build
MBX_DISABLE=1 cargo test
MBX_DISABLE=1 cargo clippy --all-targets -- -D warnings
```

If bypassed Cargo succeeds where the wrapper fails, or mbx introduces a papercut, please start a
[mr-boxington Discussion](https://github.com/jdx/mr-boxington/discussions).
Include the repository and commit, operating system, `mbx --version`,
`mbx doctor`, and both commands and their output. Before posting, redact
secrets, absolute cache paths, remote URLs, namespaces, and other sensitive or
identifying details.
