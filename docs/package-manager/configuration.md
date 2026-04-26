# Configuration

aube reads pnpm-compatible configuration from project `.npmrc`, user `.npmrc`,
`aube-workspace.yaml`, environment variables, and supported CLI flags. Existing
`pnpm-workspace.yaml` files are migration inputs.

## Defaults worth knowing

| Area | Default | Why it matters |
| --- | --- | --- |
| Linker | `nodeLinker=isolated` | Keeps transitive dependencies scoped to the packages that declared them. |
| Package imports | `packageImportMethod=auto` | Uses reflinks, hardlinks, or copies depending on filesystem support. |
| New releases | `minimumReleaseAge=1440` | Avoids installing versions published in the last 24 hours by default. |
| Exotic transitive deps | `blockExoticSubdeps=true` | Blocks transitive git and tarball dependencies unless you opt out. |
| Dependency scripts | approval required | Build scripts in dependencies stay skipped until approved. |
| Jailed builds | `jailBuilds=false` | Opt in to running approved dependency scripts with a restricted env, temporary `HOME`, and native macOS jail. |
| Auto-install before scripts | enabled | `aube run`, `aube test`, and `aube exec` repair stale installs first. |

## .npmrc

```ini
registry=https://registry.npmjs.org/
auto-install-peers=true
strict-peer-dependencies=false
node-linker=isolated
package-import-method=auto
```

See the [settings reference](/settings/) for the full list — each entry lists its `.npmrc` key alongside the other sources.

## Workspace YAML

```yaml
nodeLinker: isolated
minimumReleaseAge: 1440
publicHoistPattern:
  - "*eslint*"
jailBuilds: true
jailBuildPermissions:
  "@vendor/*":
    env:
      - SHARP_DIST_BASE_URL
    write:
      - ~/.cache/sharp
jailBuildExclusions:
  - "@legacy-native/*"
```

See the [settings reference](/settings/) — workspace YAML keys are listed per setting.
The jail-related keys are described in [Jailed builds](/package-manager/jailed-builds).

## Environment variables

pnpm-compatible `NPM_CONFIG_*` aliases are supported:

```sh
NPM_CONFIG_REGISTRY=https://registry.example.test aube install
NPM_CONFIG_NODE_LINKER=hoisted aube install
```

See the [settings reference](/settings/) — environment variables are listed per setting.

## CLI flags

CLI flags take precedence for the settings they expose:

```sh
aube install --node-linker=hoisted
aube install --network-concurrency=32
aube install --resolution-mode=time-based
```

See the [settings reference](/settings/) — CLI flags are listed per setting.

## Inspecting config

```sh
aube config get registry
aube config set auto-install-peers false
aube config list --json
```

## `package.json` — `pnpm.*` and `aube.*` namespaces

aube reads pnpm's `package.json` config keys so existing projects keep
working unchanged. Every key under `pnpm.*` is also accepted under
`aube.*` for projects that want to declare aube-native config without
piggy-backing on the pnpm namespace:

```json
{
  "aube": {
    "overrides": { "lodash": "4.17.21" },
    "catalog": { "react": "^18.0.0" },
    "supportedArchitectures": { "os": ["current", "linux"] },
    "onlyBuiltDependencies": ["sharp"],
    "patchedDependencies": { "foo@1.0.0": "patches/foo.patch" },
    "peerDependencyRules": { "ignoreMissing": ["react-native"] }
  }
}
```

Merge semantics when both namespaces are present:

- **Map-valued keys** (`overrides`, `catalog`, `catalogs`,
  `patchedDependencies`, `allowBuilds`, `allowedDeprecatedVersions`,
  `packageExtensions`, `peerDependencyRules.allowedVersions`):
  `aube.*` wins on key conflict; disjoint keys from either namespace
  merge.
- **List-valued keys** (`onlyBuiltDependencies`,
  `neverBuiltDependencies`, `ignoredOptionalDependencies`,
  `peerDependencyRules.ignoreMissing`, `peerDependencyRules.allowAny`,
  `updateConfig.ignoreDependencies`, `supportedArchitectures.{os,cpu,libc}`):
  entries from both namespaces union.
- Top-level npm-standard keys (`overrides`, `packageExtensions`,
  `allowedDeprecatedVersions`, `updateConfig`) still take highest
  precedence, so the `aube.*` alias doesn't change existing npm /
  pnpm precedence rules — it only adds a second namespace that beats
  `pnpm.*` but loses to the top-level form.
