---
description: Configure a Bun-compatible security scanner in aube and understand bootstrapping, failures, and process boundaries.
---

# Security scanner

aube ships a drop-in implementation of [Bun's Security Scanner
API](https://bun.sh/docs/pm/security-scanner-api). Point
`securityScanner` at the same npm package you'd put in Bun's
`bunfig.toml#install.security.scanner` and aube loads the module
through a `node` bridge that adapts Bun's in-process plugin
contract to a subprocess. The reference scanner template at
[oven-sh/security-scanner-template](https://github.com/oven-sh/security-scanner-template)
and the production scanner at
[`@socketsecurity/bun-security-scanner`](https://github.com/SocketDev/bun-security-scanner)
both run unchanged.

```yaml
# aube-workspace.yaml
securityScanner: "@acme/bun-security-scanner"
# or a path to a local scanner:
# securityScanner: ./scripts/scanner.mjs
```

Install a package-based scanner **before enabling the setting**. The gate
runs before fetching project dependencies, so it cannot bootstrap its own
scanner package:

```sh
aube add -D @acme/bun-security-scanner
```

The empty string (the default) disables the integration. Requires
**Node 22.6+** on `PATH`.

## When the scanner runs

**Post-resolve, once per command invocation.** After the resolver
returns a finalized graph and before the fetch / link phase starts,
aube extracts every resolved `(name, version)` pair — root direct
deps plus every transitive — and hands the full set to the scanner
in one `node` subprocess call. A `fatal` advisory aborts before any
tarball downloads happen.

The same gate covers `aube install` and `aube add` (since `aube
add` runs the install pipeline internally). One `node` spawn per
command invocation, regardless of how many packages are in the
graph.

Scoped private packages, `file:` / `link:` / workspace siblings,
git deps, and remote tarballs are excluded from the payload —
public-data scanners have no advisories for those. Aliased entries
(`{ "my-alias": "npm:real-pkg@^4" }`) are reported under the real
registry name `real-pkg`, not the alias.

## Authoring a scanner

A scanner is a JavaScript (or TypeScript) module that exports a
`scanner` object with a `scan({ packages })` function:

```ts
import type { Security } from "bun";

export const scanner: Security.Scanner = {
  version: "1",
  async scan({ packages }) {
    const advisories: Security.Advisory[] = [];
    for (const p of packages) {
      // packages[i].name    — registry name (alias-resolved)
      // packages[i].version — resolved version, e.g. "4.17.21"
      if (await isMalicious(p.name, p.version)) {
        advisories.push({
          level: "fatal",
          package: p.name,
          description: "Reported as malicious",
          url: `https://example.org/${p.name}`,
        });
      }
    }
    return advisories;
  },
};
```

The example assumes an `isMalicious(name, version)` function backed by your
scanner's advisory source.

**Levels**:

- `fatal` — aborts the install with
  `ERR_AUBE_SECURITY_SCANNER_FATAL` (exit 48).
- `warn` — emits `WARN_AUBE_SECURITY_SCANNER_FINDING` and lets
  the install proceed.
- Anything else — logged at debug level and otherwise ignored
  (future-proof for additional levels).

**Return shape**: Bun's docs specify the return value is
`Advisory[]`. aube also accepts `{ advisories: [...] }` as a
friendly fallback for scanners that wrap their result.

The published `@types/bun` package ships the canonical
`Bun.Security.Scanner` / `Bun.Security.Package` /
`Bun.Security.Advisory` types — install it as a dev dep when
authoring a TypeScript scanner.

## Bun runtime APIs aube shims

Real published scanners use a small but specific slice of the Bun
runtime. The bridge ships shims so they work unchanged:

| Bun API | aube shim |
|---|---|
| `import Bun from 'bun'` | Resolves to an aube virtual module via a Node `module.register()` loader hook. `globalThis.Bun` is also populated. |
| `Bun.env` | Alias for `process.env`. |
| `Bun.file(path)` | Returns an object with `.exists()`, `.text()`, `.json()`, `.arrayBuffer()`, `.bytes()`. |
| `Bun.write(path, data)` | Writes a file (supports strings, ArrayBuffer, TypedArray, BunFile-like objects, or anything JSON-serializable). |
| `Bun.semver.satisfies(version, range)` | Delegates to the project's `semver` npm package (near-universal transitive dep). Falls back to exact-equality comparison with a one-time stderr warning if `semver` isn't resolvable. |

That surface covers everything the oven-sh template
(`Bun.semver.satisfies`) and the Socket scanner (`Bun.env`,
`Bun.file`) actually call.

## Differences from Bun

- Requires **Node 22.6+** so the bridge can pass
  `--experimental-strip-types` to load `.ts` scanner entrypoints
  directly (Socket's package, for example, ships raw TypeScript
  via `"exports": "./src/index.ts"` with no build step).
- Bun-runtime APIs outside the shim — `Bun.spawn`, `Bun.password`,
  `Bun.serve`, the web framework, the test runner — throw at
  runtime. The bridge surfaces this as
  `ERR_AUBE_SECURITY_SCANNER_FAILED` and the install **fails
  closed** (see below).
- A `fatal` advisory on `aube add` exits non-zero after the manifest may
  have changed. Inspect `git diff -- package.json` and remove only the rejected
  dependency edit if you do not want to keep it.

## Failure semantics

**Fail closed** on any scanner failure: `node` missing on PATH,
scanner module unresolvable in `node_modules`, non-zero exit, 30
second timeout, unparseable JSON output, scanner throws. A
configured scanner that can't run is treated as a refusal —
silently bypassing on failure would defeat the entire point of
opting in.

Escape hatch: set `securityScanner = ""` to disable the
integration. Operators bootstrapping a project (the scanner
package isn't in `node_modules` on first install) or recovering
from a broken scanner can unset, complete the install, then
re-enable.

## Performance

The bridge starts one Node process and sends the resolved graph in a single
batch. Cost depends on scanner startup and any requests the scanner makes.
Warm installs that return before resolution do not run this scanner; do not
use it as evidence that an unchanged install was rescanned.

## Scanner process boundary

The scanner is executable project code. It is not run inside the dependency
build jail; only the named environment variables below are removed. Choose
and review the scanner accordingly.

- The subprocess environment is scrubbed of `AUBE_AUTH_TOKEN`,
  `NPM_TOKEN`, `NODE_AUTH_TOKEN`, `GITHUB_TOKEN`, and `GH_TOKEN`
  before exec. This removes those values from `process.env`; it does not restrict
  filesystem reads or the scanner's other access.
- `kill_on_drop(true)` on the spawn ensures a hung scanner is
  SIGKILLed at the 30 s timeout instead of leaking as an orphan
  process.
- The scanner module is loaded with the *project root* as `cwd`,
  not aube's working directory. Module resolution from the
  scanner uses the project's `node_modules`.
- The bridge writes three short `.mjs` files (the shim, the
  loader hook, the runner) to a fresh `tempfile::TempDir` per
  invocation. The temp dir is cleaned up when the subprocess
  exits.

## Configuring an existing Bun scanner

Most Bun-compatible scanners are published as npm packages with a
single `securityScanner = "<package-name>"` line. Some accept
extra configuration via environment variables (Socket, for
example, reads `SOCKET_SECURITY_API_KEY` from `Bun.env`). Set
those in the parent shell environment — aube's bridge passes
`process.env` through (minus the token scrub list above).

```sh
export SOCKET_SECURITY_API_KEY="…"
aube install   # scanner sees SOCKET_SECURITY_API_KEY via Bun.env
```

## Related settings

- [`securityScanner`](/settings/#setting-securityscanner) — the
  module spec.
- [`paranoid`](/settings/#setting-paranoid) — does **not**
  currently enable a default scanner. If you want a scanner
  running in CI, configure it explicitly.

## Related codes

- `ERR_AUBE_SECURITY_SCANNER_FATAL` (exit 48) — scanner returned
  a fatal advisory.
- `ERR_AUBE_SECURITY_SCANNER_FAILED` — scanner couldn't run
  (fail-closed contract).
- `WARN_AUBE_SECURITY_SCANNER_FINDING` — scanner returned a
  warn-level advisory.
