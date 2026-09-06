---
description: Inspect package contents, publish tarballs, attach provenance, and manage dist-tags and deprecations.
---

# Publishing

Use aube to inspect package contents, publish to your configured npm registry,
and manage dist-tags and deprecations. Authenticate first using
[registry configuration](/package-manager/registry-auth).

Start with a dry run to check the package contents and destination:

```sh
aube publish --dry-run --json
```

## Pack

```sh
aube pack
aube pack --dry-run
aube pack --json
aube pack --pack-destination dist
```

`pack` applies npm-style file selection: `files` field first, otherwise
standard ignore rules, with `package.json`, README, LICENSE, and the `main`
entry always included.

## Publish

```sh
aube publish
aube publish ./package-1.0.0.tgz
aube publish --tag next
aube publish --access public
aube publish --dry-run --json
```

Publishing a prebuilt tarball skips lifecycle scripts and working-tree
cleanliness checks. Publishing a package directory retains both behaviors.

Workspace fanout uses the global workspace selectors:

```sh
aube -r publish
aube -F '@acme/*' publish
```

## Provenance

```sh
aube publish --provenance
```

Provenance requires an OIDC-capable CI environment such as GitHub Actions with
`id-token: write`. aube signs a SLSA in-toto statement via Sigstore and
attaches the bundle to the publish body.

## Dist-tags

```sh
aube dist-tag add @acme/widget@1.2.0 stable
aube dist-tag ls @acme/widget
aube dist-tag rm @acme/widget stable
```

## Deprecate and unpublish

```sh
aube deprecate '@acme/widget@<2' "Use @acme/widget 2 or newer"
aube undeprecate '@acme/widget@<2'
aube unpublish @acme/widget@1.0.0 --dry-run
```

Replace the example package with one you own. Deprecation leaves a package
available with a warning; unpublishing removes a version or package from the
registry. Whole-package unpublish requires `--force`. See
[`aube unpublish`](/cli/unpublish) before removing a published package.
