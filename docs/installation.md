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

::: info
`--locked` makes cargo honor the committed `Cargo.lock` so you get the
same dependency versions CI built against. The compiled binary lands in
`~/.cargo/bin/aube`.
:::

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

::: warning
The `preinstall` script drops the platform-appropriate native binary
into place. If you install with `--ignore-scripts`, that step is
skipped and every `aube` invocation goes through a node shim instead
— which defeats the whole point of having a fast, native CLI. It also
won't work in offline/air-gapped caches. Use any other install method
for those environments.
:::

## Ubuntu (PPA)

**Supported:** Ubuntu 26.04 (resolute).

Aube publishes signed `.deb` packages to the Launchpad PPA
[`ppa:jdxcode/aube`](https://launchpad.net/~jdxcode/+archive/ubuntu/aube):

```sh
sudo apt install -y software-properties-common   # if add-apt-repository isn't already available
sudo add-apt-repository -y ppa:jdxcode/aube
sudo apt install aube
```

Future upgrades go through `apt`:

```sh
sudo apt update && sudo apt install --only-upgrade aube
```

## Fedora / RHEL (COPR)

**Supported:** Fedora 42, Fedora 43, Fedora Rawhide, EPEL 9, EPEL 10
(RHEL / Rocky / Alma 9 and 10), both `x86_64` and `aarch64`.

Aube publishes RPMs to the COPR project
[`jdxcode/aube`](https://copr.fedorainfracloud.org/coprs/jdxcode/aube/):

```sh
sudo dnf copr enable jdxcode/aube
sudo dnf install aube
```

The `dnf copr` subcommand ships with `dnf-plugins-core` — install that
first on EPEL and anywhere else the plugin isn't already pulled in.
Future upgrades go through the package manager:

```sh
sudo dnf upgrade aube
```

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

Completions are powered by [`usage`](https://usage.jdx.dev), so install
that first:

```sh
mise use -g usage
```

Then render the completion script for your shell:

```sh
aube completion bash   > /etc/bash_completion.d/aube
aube completion zsh    > "${fpath[1]}/_aube"
aube completion fish   > ~/.config/fish/completions/aube.fish
```
