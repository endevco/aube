---
description: Restrict approved dependency builds with aube jails and understand filesystem, network, environment, and platform limits.
---

# Jailed dependency builds

Build approval decides whether a dependency script can run. A build jail
restricts an approved script's environment, filesystem writes, and network
access. The jail is optional and does not replace build review.

Enable it in `aube-workspace.yaml` or an existing `pnpm-workspace.yaml`:

```yaml
jailBuilds: true
```

`jailBuilds` defaults to `false`. Root lifecycle scripts are not jailed.
See [lifecycle scripts](/package-manager/lifecycle-scripts) for approvals.

## Default profile

| Capability | macOS and supported Linux systems | Windows |
| --- | --- | --- |
| Filesystem reads | Unrestricted | Unrestricted |
| Filesystem writes | Package directory and jail-owned temporary directories | No native restriction |
| Network | Denied | No native restriction |
| Environment | Scrubbed allowlist | Scrubbed allowlist |
| `HOME` | Temporary jail directory | Temporary jail directory |

::: warning Current boundary
Filesystem reads are unrestricted. A temporary `HOME` and scrubbed environment
do not prevent a script from reading a known absolute path to a credential file.
Windows currently provides environment scrubbing and a temporary home only;
aube warns that native filesystem and network enforcement are unavailable.
:::

## Grant a specific permission

If a reviewed package needs an environment variable, writable cache, or network
access, grant that permission while keeping the rest of the jail:

```yaml
jailBuilds: true
jailBuildPermissions:
  sharp:
    env:
      - SHARP_DIST_BASE_URL
    write:
      - ~/.cache/sharp
    network: true
```

| Key | Effect |
| --- | --- |
| `env` | Inherit the named variables from the parent process |
| `write` | Add paths to the native write allowlist on macOS and Linux |
| `network` | Permit network access when `true` |
| `read` | Reserved for a future read-restricted profile; reads are currently unrestricted |

Environment grants can expose secrets, and `network: true` enables network
access rather than a host-specific allowlist. Grant only what the build needs.

Package keys accept bare names, exact `name@version` pins, exact version unions,
and name wildcards such as `@scope/*`. A bare name applies to every version;
use an exact pin when the exception belongs to one reviewed release.

## Exclude a package

If a reviewed package cannot run with individual permission grants, exclude it:

```yaml
jailBuilds: true
jailBuildExclusions:
  - "legacy-native-addon@1.2.3"
```

An exclusion disables the jail for that package. It does not approve the build:
the package must still pass the active build policy. See
[`jailBuildExclusions`](/settings/#setting-jailbuildexclusions).

## Native enforcement

- **macOS:** a Seatbelt profile, applied through `sandbox-exec`, restricts writes
  and network access.
- **Linux:** Landlock write restrictions and a seccomp network filter are applied
  in the child process. The baseline requires kernel 5.19 or newer with Landlock
  ABI v2. If the requested jail cannot be enforced, the build fails rather than
  running without it. Landlock v2 does not restrict `truncate()` on otherwise
  read-only paths; that protection requires kernel 6.2 or newer.
- **Windows:** environment scrubbing and a temporary home are applied, with a
  warning about unavailable native enforcement.

The jail runs below the dependency script runner, so approved builds from
install and `aube rebuild` use the same policy.

## Environment policy

The scrubbed environment includes values needed for build tools, such as `PATH`,
`INIT_CWD`, and npm lifecycle metadata. `HOME` points to a temporary directory.
Common credential variables such as `NPM_TOKEN`, `NODE_AUTH_TOKEN`,
`GITHUB_TOKEN`, and `SSH_AUTH_SOCK` are removed unless explicitly granted.

## Diagnose a failed build

1. Confirm the package is approved with `aube ignored-builds`.
2. Read the failing script and its first error. Identify the missing variable,
   denied write, or required network access.
3. Add a package-specific permission and retry `aube rebuild`.
4. Commit the policy once the build works on the platforms your project supports.

Read [security defaults](/security) for the other install protections and
[configuration](/package-manager/configuration) for managed organization policy.
