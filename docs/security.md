# Security

aube treats supply-chain protection as a first-class concern. This page lists
every security-relevant feature, its default, and the one-line config to turn
it on or off.

To report a vulnerability, see the [security policy](https://github.com/endevco/aube/security/policy).

## The `paranoid` switch

The fastest way to opt into the strict-security posture is one line:

```yaml
paranoid: true
```

This forces every setting in the strict bundle on, regardless of how each is
configured individually:

- [`jailBuilds = true`](#jailed-lifecycle-scripts)
- [`trustPolicy = no-downgrade`](#trust-policy) (overrides explicit `off`)
- `minimumReleaseAgeStrict = true` — turns the age gate into a hard fail
  instead of "fall back to the lowest satisfying version"
- `strictStoreIntegrity = true` — fail when a tarball ships without
  `dist.integrity` instead of warning
- `strictDepBuilds = true` — fail the install when a dep has unreviewed
  build scripts instead of silently skipping
- [`advisoryCheck = required`](#typosquat-and-impersonation-protection) —
  fail `aube add` if OSV can't be reached instead of falling back to
  download-count signal alone

Use it when you want maximum protection without listing each setting.

## Default-deny lifecycle scripts

Lifecycle scripts (`preinstall`, `install`, `postinstall`) are the sharpest
supply-chain edge in a JavaScript install. aube does not run dependency
lifecycle scripts unless you've approved them explicitly:

```yaml
# aube-workspace.yaml
allowBuilds:
  esbuild: true
  sharp: true
```

Or interactively:

```sh
aube approve-builds
```

Root-package lifecycle scripts (your own project's) still run normally — the
boundary is dependency code.

Settings: [`allowBuilds`](/settings/#setting-allowbuilds). Install adds
unreviewed build packages to `aube-workspace.yaml` (or `pnpm-workspace.yaml`
if one already exists) as `false`; approving them flips the entry to `true`.

## Jailed lifecycle scripts

When a dependency is approved to build, jailing keeps it from getting your
full filesystem, network, and environment. On macOS aube wraps the script with
a Seatbelt profile; on Linux it applies Landlock and seccomp before exec. Both
deny network access and limit writes to package and jail-owned temporary
directories. On Windows the env is scrubbed and `HOME` is redirected to a
temporary directory.

```yaml
jailBuilds: true
```

Grant narrow exceptions per-package instead of disabling the jail wholesale:

```yaml
jailBuilds: true
jailBuildPermissions:
  sharp:
    env: [SHARP_DIST_BASE_URL]
    write: ["~/.cache/sharp"]
    network: true
```

Default: `false` today, planned to flip to `true` in the next major.

Full reference: [Jailed builds](/package-manager/jailed-builds).

## Trust policy

`trustPolicy = no-downgrade` blocks installs of a version that carries weaker
trust evidence than any earlier-published version of the same package. aube
only counts the structured metadata shape npm emits after registry-side checks:

1. **npm trusted-publisher** — package was published via OIDC from a trusted
   CI provider (`_npmUser.trustedPublisher.id`).
2. **Sigstore provenance** — package was published with `npm publish
   --provenance` (`dist.attestations.provenance.predicateType` with an SLSA
   provenance URI).

This install-time policy validates the registry metadata shape; it does not
cryptographically verify the attached attestation bundle.

A trust downgrade may indicate a supply-chain incident: publisher account
takeover, repository tampering, or a malicious co-maintainer publishing
without the original CI flow.

```yaml
trustPolicy: no-downgrade
```

Exempt specific packages or versions when needed (only exact versions, no
ranges):

```yaml
trustPolicyExclude:
  - "@vendor/legacy-pkg"            # all versions
  - "old-thing@1.0.0"                # one version
  - "things@1.0.0 || 1.0.1"          # version union
  - "is-*"                           # name glob (no version)
```

Default: `no-downgrade`. Set `trustPolicy: off` to disable, or use
`trustPolicyExclude` for per-package opt-outs.

Settings: [`trustPolicy`](/settings/#setting-trustpolicy),
[`trustPolicyExclude`](/settings/#setting-trustpolicyexclude),
[`trustPolicyIgnoreAfter`](/settings/#setting-trustpolicyignoreafter).

## Minimum release age

Wait a configurable period before installing newly published versions. Catches
typo-squat and dependency-confusion attacks that get unpublished within hours.

```yaml
minimumReleaseAge: 4320  # 3 days
```

`minimumReleaseAgeStrict: true` fails the install when no version satisfies
the range; otherwise the resolver falls back to the lowest satisfying version
ignoring the cutoff for that pick only.

Default: `0` (disabled).

Settings: [`minimumReleaseAge`](/settings/#setting-minimumreleaseage),
[`minimumReleaseAgeExclude`](/settings/#setting-minimumreleaseageexclude),
[`minimumReleaseAgeStrict`](/settings/#setting-minimumreleaseagestrict).

## Typosquat and impersonation protection

`aube add` checks every package on the command line before adding it to your
manifest. Transitive deps and packages already in the lockfile are not
re-checked — the gate is the moment of human intent, not every reinstall.

Two signals, with different response levels:

**Known-malicious advisories.** aube batch-queries [OSV](https://osv.dev) for
`MAL-*` advisories on every name about to be added. A hit fails the install
with `ERR_AUBE_MALICIOUS_PACKAGE` and a link to the advisory. Confirmed
malicious isn't a judgement call — this is a hard block, not a prompt. If
the OSV API can't be reached, the default (`advisoryCheck: on`) warns and
continues; `advisoryCheck: required` upgrades that to a fail-closed
`ERR_AUBE_ADVISORY_CHECK_FAILED` so CI can tell a network outage from a
confirmed-malicious advisory.

**Low download count.** A typosquat or impersonation has approximately zero
installs on day one regardless of how cleverly it's named, so a
download-count floor catches the long tail of squats that haven't been
reported yet. Below the threshold, aube prompts for confirmation:

```
aube add supabase-javascript

  ⚠ supabase-javascript looks suspicious:
    • 3 downloads last week (threshold: 1000)
  Continue adding supabase-javascript? [y/N]
```

In non-interactive contexts the prompt becomes a hard refusal with
`ERR_AUBE_LOW_DOWNLOAD_PACKAGE` unless `--allow-low-downloads` is passed.
Scoped private packages and workspace deps are skipped (no public registry
signal → no false positive).

```yaml
advisoryCheck: on            # default; fail open on network error
lowDownloadThreshold: 1000   # weekly downloads, 0 disables
```

Set `advisoryCheck: required` to fail closed when OSV can't be reached —
appropriate for hardened CI, included in `paranoid: true`. Set
`advisoryCheck: off` or `lowDownloadThreshold: 0` to disable either check
independently.

Settings: [`advisoryCheck`](/settings/#setting-advisorycheck),
[`lowDownloadThreshold`](/settings/#setting-lowdownloadthreshold).

## Block exotic transitive dependencies

Reject transitive dependencies that resolve to `git+`, `file:`, or direct
tarball URLs — those skip the registry and its integrity verification. Direct
deps you pin yourself in `package.json` are still allowed.

```yaml
blockExoticSubdeps: true   # default
```

Settings: [`blockExoticSubdeps`](/settings/#setting-blockexoticsubdeps).

## Tarball integrity

Every registry tarball is verified against the SHA-512 hash recorded in the
packument's `dist.integrity` field before it is added to the store. Mismatches
fail the install loudly. The hash is preserved in the lockfile, so subsequent
installs reverify on every fetch.

The content-addressable store itself uses BLAKE3 for the on-disk index — fast
to compute and immune to length-extension. Linked `node_modules` files are
reflinks (APFS/btrfs), hardlinks (ext4), or copies; none of those paths can
modify the canonical store entry.

## Auth tokens

Registry tokens are read from `.npmrc` (the npm convention) or environment
variables (`NPM_TOKEN`, `AUBE_AUTH_TOKEN`, etc.) and **never written to the
lockfile, tarball cache, or logs**. `aube login` and `aube logout` manage
tokens via the standard npm config file.

Inside jailed lifecycle scripts, common token env vars (`NPM_TOKEN`,
`NODE_AUTH_TOKEN`, `GITHUB_TOKEN`, `SSH_AUTH_SOCK`, `AWS_*`, etc.) are
scrubbed from the script environment unless explicitly granted via
`jailBuildPermissions`.

## Pluggable security scanner

`securityScanner` runs a [Bun-compatible security scanner](https://bun.sh/docs/pm/security-scanner-api)
against the packages an install (or `add`) is about to introduce.
Drop-in compatible with the existing Bun scanner ecosystem — point
the setting at the same npm package name you'd put in
`bunfig.toml#install.security.scanner` and aube loads the module
through a `node` bridge that adapts Bun's in-process plugin
contract to a subprocess + JSON-over-stdio shape.

**Fires post-resolve.** Once the resolver has produced a finalized
graph and before the fetch/link phase starts, aube extracts every
resolved `(name, version)` pair (root direct deps + every
transitive — same view Bun's scanner gets) and hands them to the
scanner. A `fatal` advisory aborts before any tarball downloads
happen. One `node` process per command invocation, regardless of
how many packages are in the graph.

`aube add` doesn't have a separate scanner hook — it mutates
`package.json`, then runs the install pipeline where this gate
fires.

```yaml
# aube-workspace.yaml
securityScanner: "@acme/bun-security-scanner"
# or a path to a local scanner:
# securityScanner: ./scripts/scanner.mjs
```

The scanner module exports the standard Bun shape:

```js
export const scanner = {
  version: '1',
  async scan({ packages }) {
    // packages: [{ name, version }, ...]
    return [
      {
        level: 'fatal',                 // or 'warn'
        package: 'evil-pkg',
        description: 'Known malicious package',
        url: 'https://socket.dev/...',
      },
    ];
  },
};
```

Bun's docs specify the return value is `Advisory[]`. Aube also
accepts `{ advisories: [...] }` for friendliness.

A `fatal` advisory fails the install with `ERR_AUBE_SECURITY_SCANNER_FATAL`.
A `warn` advisory surfaces via `WARN_AUBE_SECURITY_SCANNER_FINDING`
and the install continues. Any other level is logged at debug only.

**Bun runtime APIs aube shims**:

- `import Bun from 'bun'` resolves to aube's virtual module via a
  Node module-loader hook. `globalThis.Bun` is also populated.
- `Bun.env` (= `process.env`), `Bun.file(path)` with
  `.exists() / .text() / .json() / .arrayBuffer() / .bytes()`,
  and `Bun.write(path, data)`.
- `Bun.semver.satisfies(version, range)` delegates to the
  project's `semver` npm package (install it as a dev dep for
  full compat; aube falls back to exact-equality with a warning
  if it's not present).

That's the surface real published scanners actually use — the
[oven-sh template](https://github.com/oven-sh/security-scanner-template)
and [`@socketsecurity/bun-security-scanner`](https://github.com/SocketDev/bun-security-scanner)
both run unchanged.

**Differences from Bun**:

- Requires **Node 22.6+** so the bridge can pass
  `--experimental-strip-types` to load `.ts` scanner entrypoints
  directly (Socket's package, for example, ships raw TypeScript
  with `"exports": "./src/index.ts"`).
- Bun-runtime APIs outside the shim (`Bun.spawn`, `Bun.password`,
  `Bun.serve`) will throw; the bridge surfaces this as
  `ERR_AUBE_SECURITY_SCANNER_FAILED` and the install **fails closed**.
- A `fatal` advisory on `aube add` exits non-zero with
  `package.json` still mutated (Bun behaves the same way). Revert
  via `git checkout package.json` if you don't want to keep the
  edit.

Failure modes — `node` missing, scanner module unresolvable,
non-zero exit, timeout (30s), unparseable JSON — all **fail closed**
with `ERR_AUBE_SECURITY_SCANNER_FAILED`. A configured scanner that
can't run is treated as a refusal; silent bypass would defeat the
point of opting in. Operators bootstrapping a project (scanner npm
package not yet installed) or recovering from a broken scanner can
set `securityScanner: ""` in workspace yaml to disable the
integration until the scanner is back.

Skipped entries: anything with a `local_source` in the resolved
graph — `file:`, `link:`, workspace siblings, git fetches, remote
tarballs. The scanner has no public-registry data for these, and
including them would force every scanner author to special-case
non-registry rows. Aliased entries
(`{ "my-alias": "npm:real-pkg@^4" }`) are reported under
`real-pkg` (the registry name), not the alias.

Empty string (the default) disables the integration entirely.

Settings: [`securityScanner`](/settings/#setting-securityscanner).

## Auditing installed dependencies

```sh
aube audit                # list known CVEs at moderate+ severity
aube audit --audit-level high
aube audit --fix          # write package.json overrides to patched versions
aube audit --json | jq    # machine-readable for CI
```

Same advisory data source as `npm audit` and `pnpm audit`; same response
schema.

## Recommended baseline

For most projects, the following is a good starting point:

```yaml
# aube-workspace.yaml
paranoid: true             # bundles jailBuilds, no-downgrade, strict gates
allowBuilds:
  esbuild: true
  sharp: true
  # ...whatever your project actually needs to build
```

`trustPolicy=no-downgrade` and `minimumReleaseAge: 1440` (24h) are already
default-on; `paranoid: true` adds the rest of the bundle on top. Pair this
with `aube audit` in CI so a newly disclosed CVE fails the build instead of
silently shipping.
