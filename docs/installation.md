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

## Ubuntu (PPA)

Aube publishes signed `.deb` packages to the Launchpad PPA
[`ppa:jdxcode/aube`](https://launchpad.net/~jdxcode/+archive/ubuntu/aube):

```sh
sudo add-apt-repository -y ppa:jdxcode/aube
sudo apt update
sudo apt install aube
```

This installs `aube`, plus `aubr` and `aubx` symlinks (the multicall
shims for `aube run` and `aube dlx`) into `/usr/bin`. Future upgrades
go through `apt`:

```sh
sudo apt update && sudo apt install --only-upgrade aube
```

Currently the PPA only builds for **Ubuntu 26.04 (resolute)**. On
older releases, `add-apt-repository` will succeed but `apt update`
returns a 404 because no `Release` file is published for that series —
use mise, `cargo install`, or the npm package instead.

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
