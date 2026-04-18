# Installation

## Recommended: mise

Install aube globally with mise:

```sh
mise use -g aube
```

This installs `aube` on your PATH and lets mise manage future upgrades.

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
