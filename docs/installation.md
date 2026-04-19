# Installation

## Recommended: mise

Install aube globally with mise:

```sh
mise use -g aube
```

This installs `aube` on your PATH and lets mise manage future upgrades.

## From crates.io

If you already have a Rust toolchain installed, you can install the
latest released `aube` from crates.io:

```sh
cargo install aube --locked
```

`--locked` makes cargo honor the committed `Cargo.lock` so you get the
same dependency versions CI built against. The compiled binary lands in
`~/.cargo/bin/aube`.

## From Homebrew

Homebrew/core does not accept beta releases, so aube is published from
the Endev tap for now:

```sh
brew install endevco/tap/aube
```

The tap formula builds from source and installs shell completions.

## From npm

aube is also published on npm as `@endevco/aube`:

```sh
npm install -g @endevco/aube
# or
npx @endevco/aube --version
```

Because the install happens via `preinstall`, this does not work with
`--ignore-scripts` or in offline/air-gapped caches. Prefer mise or
`cargo install` for those environments.

## From source

If you want to build the current checkout yourself, use the standard source
build flow:

```sh
git clone https://github.com/endevco/aube
cd aube
cargo install --path crates/aube
```

This installs the `aube` binary into `~/.cargo/bin`.

## Verify

```sh
aube --version
```

## Shell completions

```sh
aube completion bash   > /etc/bash_completion.d/aube
aube completion zsh    > "${fpath[1]}/_aube"
aube completion fish   > ~/.config/fish/completions/aube.fish
```
