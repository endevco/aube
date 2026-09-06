---
description: Install aube with mise, Homebrew, npm, Cargo, Linux packages, or from source, and configure shell completions.
---

# Installation

Choose one installation method, then confirm `aube --version` works in your
shell. Release archives include `aube` plus the `aubr` and `aubx` shortcuts.
Node.js is needed to run JavaScript; aube can resolve a
[project-pinned Node version](/package-manager/node-runtime) for you.

| Method | Use it when |
| --- | --- |
| [mise](#recommended-mise) | You want aube and Node managed together |
| [Homebrew](#from-homebrew) | You use Homebrew on macOS or Linux |
| [npm](#from-npm) | You want installation through an existing npm setup |
| [Cargo](#from-crates-io) | You have a Rust toolchain and want to compile the release |
| [Ubuntu PPA](#ubuntu-ppa) / [Fedora COPR](#fedora-rhel-copr) | You prefer system packages on a supported distribution |
| [Source](#from-source) | You want to build a checkout |


## Recommended: mise

Install aube globally with mise:

```sh
mise use -g aube
```

This installs `aube` on your PATH and lets mise manage future upgrades.

::: tip
We recommend mise because it can manage `aube` and your
[Node.js runtime](https://mise.jdx.dev/lang/node.html) from the same
toolchain. If your projects already pin Node through `package.json`
(`devEngines.runtime`) or files such as `.nvmrc` and
`.node-version`, opt mise into reading those idiomatic version files:

```sh
mise settings add idiomatic_version_file_enable_tools node
```
:::

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

aube is published from the jdx tap until it lands in homebrew-core:

```sh
brew install jdx/tap/aube
```

The tap provides the formula and shell completions.

## From npm

aube is also published on npm as `@endevco/aube`:

```sh
npm install -g --ignore-scripts=false @endevco/aube
npx --ignore-scripts=false @endevco/aube --version
```

::: warning
The npm package relies on its `preinstall` script to fetch the
platform-specific native binary and wire up the `aube`, `aubr`, and `aubx`
commands. That native binary is what gives aube its startup and install
performance; without the script, npm can leave the package installed
without working commands. The npm commands above pass
`--ignore-scripts=false` so it still works for users with
`ignore-scripts=true` in their npm config.

We recommend installing with mise if you want the native binary without npm
lifecycle-script behavior.
:::

## Ubuntu (PPA)

**Supported:** Ubuntu 26.04 (resolute).

aube publishes signed `.deb` packages to the Launchpad PPA
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

**Supported:** Fedora 43, Fedora 44, Fedora Rawhide, EPEL 10
(RHEL / Rocky / Alma 10), both `x86_64` and `aarch64`.

aube publishes RPMs to the COPR project
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
git clone https://github.com/aubepkg/aube
cd aube
cargo install --path crates/aube
```

This installs the `aube` binary into `~/.cargo/bin`.

## GitHub Actions

Use [`jdx/aube-action`](https://github.com/jdx/aube-action) to install the native
binary and optionally Node.js. See [CI and containers](/package-manager/ci)
for a complete workflow and advice on frozen installs and caches.

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

Install completion files for your shell:

```sh
aube completion bash --install
aube completion zsh --install
aube completion fish --install
```

Run the command for the shell you use. It installs completion files for `aube`,
`aubr`, and `aubx` and prints any shell-specific setup still needed. It does not
edit your shell startup file. Omit `--install` to print the completion script
instead. See [`aube completion`](/cli/completion) for PowerShell and per-binary
options.

`aube run <TAB>` and `aubr <TAB>` complete the scripts declared in the
`package.json` nearest your current directory, with each script's command
shown as the description. A `-C` earlier on the line isn't taken into
account — the scripts offered are always the current directory's.

## Next step

[Run your first project script](/getting-started#_2-run-a-project-script), or
choose a migration guide from the [documentation map](/guide).
