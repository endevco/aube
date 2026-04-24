pub mod bun;
pub mod dep_path_filename;
pub mod graph_hash;
pub mod merge;
pub mod npm;
pub mod pnpm;
pub mod yarn;

pub use merge::{MergeReport, merge_branch_lockfiles};

use smallvec::SmallVec;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Most npm packages declare zero or one entry in `os`, `cpu`,
/// `libc`. Two inline `SmallVec` slots cover empty on construction
/// (zero heap alloc) and one-entry push (still zero heap) for ~99%
/// of lockfile entries.
pub type PlatformList = SmallVec<[String; 2]>;

/// Represents a resolved dependency graph from any lockfile format.
#[derive(Debug, Clone, Default)]
pub struct LockfileGraph {
    /// Direct dependencies of the root project (and workspace packages).
    /// Key: importer path (e.g., "." for root), Value: list of (name, version) pairs.
    pub importers: BTreeMap<String, Vec<DirectDep>>,
    /// All resolved packages.
    pub packages: BTreeMap<String, LockedPackage>,
    /// Per-graph settings that round-trip through the lockfile header
    /// (pnpm v9's `settings:` block). Don't affect graph structure;
    /// stamped into the YAML when writing and read back when parsing,
    /// so subsequent installs see the same resolution-mode state.
    pub settings: LockfileSettings,
    /// Dependency overrides recorded in pnpm-lock.yaml's top-level
    /// `overrides:` block. Map of raw selector key → version specifier
    /// (or `npm:` alias). Keys are the user's verbatim selector
    /// strings — bare name, `foo>bar`, `foo@<2`, `**/foo`, or any
    /// combination. Round-tripped so subsequent installs can detect
    /// override drift on a string-compare of the key+value without
    /// re-running the resolver. The resolver parses these into
    /// `override_rule::OverrideRule`s at the start of each resolve
    /// pass.
    pub overrides: BTreeMap<String, String>,
    /// Names listed in the root manifest's `pnpm.ignoredOptionalDependencies`.
    /// The resolver drops entries in this set from every `optionalDependencies`
    /// map before enqueueing, matching pnpm's read-package hook. Round-tripped
    /// through pnpm-lock.yaml's top-level `ignoredOptionalDependencies:` list
    /// so drift detection can notice when the user edits the field.
    pub ignored_optional_dependencies: BTreeSet<String>,
    /// Per-package publish timestamps, keyed by canonical `name@version`
    /// (no peer suffix). Round-trips through pnpm-lock.yaml's top-level
    /// `time:` block so `--resolution-mode=time-based` can compute a
    /// `publishedBy` cutoff from packages already in the lockfile
    /// without re-fetching packuments.
    pub times: BTreeMap<String, String>,
    /// Optional dependencies the resolver intentionally skipped on the
    /// platform that wrote this lockfile (either filtered by
    /// `os`/`cpu`/`libc`, or named in
    /// `pnpm.ignoredOptionalDependencies`). Keyed by importer path,
    /// inner map is name → specifier captured from `package.json` at
    /// resolve time.
    ///
    /// Drift detection uses this to distinguish "user just added a new
    /// optional dep" (which is real drift) from "this optional was
    /// already considered and consciously dropped on this platform"
    /// (which is *not* drift). Without it, every `--frozen-lockfile`
    /// install on a platform that skipped a fixture would hard-fail.
    pub skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>>,
    /// Resolved catalog entries, mirroring pnpm v9's top-level
    /// `catalogs:` block. Outer key is the catalog name (`default` for
    /// the unnamed `catalog:` field in `pnpm-workspace.yaml`); inner key
    /// is the package name. Each entry pairs the original specifier
    /// from the workspace catalog with the version the resolver chose
    /// for it. Round-tripped through the lockfile so drift detection
    /// can fire when a catalog spec changes without re-resolving.
    pub catalogs: BTreeMap<String, BTreeMap<String, CatalogEntry>>,
    /// bun's top-level `configVersion` — a second format counter bun
    /// added alongside `lockfileVersion` to track its own config-
    /// schema changes. Only the bun parser/writer ever touches this;
    /// other formats leave it `None`. Round-tripping the parsed
    /// value keeps the writer from silently downgrading the field
    /// (e.g. from `2` back to `1`) when bun bumps it in a future
    /// release.
    pub bun_config_version: Option<u32>,
    /// Top-level `patchedDependencies:` block mirrored by bun 1.1+ and
    /// pnpm 9+. Key: selector (`lodash@4.17.21`), value: relative patch
    /// file path (`patches/lodash@4.17.21.patch`). Round-tripped
    /// verbatim so a parse/write cycle doesn't silently drop user
    /// patches from the lockfile.
    pub patched_dependencies: BTreeMap<String, String>,
    /// Top-level `trustedDependencies:` block (bun) — a package-name
    /// allowlist for lifecycle script execution. Preserved so
    /// re-emitting a bun.lock doesn't strip the allowlist and cause
    /// subsequent installs to skip scripts the user explicitly
    /// approved.
    ///
    /// Kept as a `Vec` (not a set) so bun's original order round-trips
    /// byte-identically; bun emits the list in insertion order. The
    /// parser is responsible for deduping if the source lockfile
    /// carried a duplicate.
    pub trusted_dependencies: Vec<String>,
    /// Top-level lockfile fields that aren't explicitly modeled on
    /// `LockfileGraph`. Populated by per-format parsers on best-effort
    /// basis so the writer can re-emit blocks a future lockfile
    /// version might add (or ones we haven't promoted to typed fields
    /// yet) without silently stripping them on round-trip. Each
    /// parser/writer is responsible for emitting values in its
    /// format's native serialization.
    pub extra_fields: BTreeMap<String, serde_json::Value>,
    /// Per-workspace-importer extras keyed by importer path (`""` for
    /// root in bun, `"."` for others). Stores anything in the
    /// workspace entry the typed model doesn't capture so a parse/
    /// write cycle doesn't drop fields the user (or bun) wrote there.
    pub workspace_extra_fields: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// One entry in a lockfile catalog: the workspace-declared range and the
/// resolved version. Mirrors pnpm v9's `catalogs:` block exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub specifier: String,
    pub version: String,
}

/// Per-graph settings that mirror pnpm v9's `settings:` header.
/// Extend as more knobs become round-trip-aware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockfileSettings {
    /// pnpm's `auto-install-peers` — when false the resolver leaves
    /// unmet peers alone (just warns) instead of dragging them in.
    pub auto_install_peers: bool,
    /// pnpm's `exclude-links-from-lockfile` — not yet honored by aube
    /// but round-tripped for lockfile compatibility.
    pub exclude_links_from_lockfile: bool,
    /// pnpm's `lockfile-include-tarball-url` — when true the writer
    /// emits the full registry tarball URL in each package's
    /// `resolution.tarball:` field alongside `integrity:`. Makes the
    /// lockfile self-contained so air-gapped installs don't need to
    /// derive the URL from `.npmrc`. Round-tripped through the
    /// `settings:` header so it survives parse/write cycles without
    /// re-reading `.npmrc`.
    pub lockfile_include_tarball_url: bool,
}

impl Default for LockfileSettings {
    fn default() -> Self {
        Self {
            auto_install_peers: true,
            exclude_links_from_lockfile: false,
            lockfile_include_tarball_url: false,
        }
    }
}

/// A direct dependency of a workspace importer.
#[derive(Debug, Clone)]
pub struct DirectDep {
    pub name: String,
    /// The dep_path key in the lockfile (e.g., "is-odd@3.0.1")
    pub dep_path: String,
    pub dep_type: DepType,
    /// The specifier as written in package.json at the time the lockfile was
    /// generated (e.g., `"^4.17.0"`). Used by drift detection to compare against
    /// the current manifest. Only populated by formats that record it
    /// (pnpm-lock.yaml v9). `None` for npm/yarn/bun lockfiles.
    pub specifier: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepType {
    Production,
    Dev,
    Optional,
}

/// Non-registry source for a locked package.
///
/// When a package comes from a local path (via `file:` or `link:` in
/// `package.json`) it doesn't have a tarball URL or integrity hash, so we
/// record the source separately and let the linker materialize it
/// on-the-fly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSource {
    /// `file:<dir>` — a directory on disk whose contents should be
    /// hardlink-copied into the virtual store like a normal package.
    /// Path is stored relative to the project root.
    Directory(PathBuf),
    /// `file:<tarball>` — a `.tgz` on disk, extracted into the virtual
    /// store the same way we extract registry tarballs.
    Tarball(PathBuf),
    /// `link:<dir>` — a plain symlink into `node_modules/<name>`, never
    /// materialized into the virtual store. Transitive deps are the
    /// target's responsibility.
    Link(PathBuf),
    /// `git+https://`, `git+ssh://`, `github:user/repo`, etc. — a
    /// remote git repo. Cloned at fetch time and imported like a
    /// `file:` directory. `url` is the normalized clone URL (what
    /// gets passed to `git clone`). `committish` is the user-written
    /// ref after `#` (branch, tag, or commit; `None` means HEAD).
    /// `resolved` is the 40-char commit SHA that `git ls-remote`
    /// pinned the ref to — the lockfile records this so repeat
    /// installs reproduce bit-for-bit.
    Git(GitSource),
    /// `https://example.com/pkg.tgz` — a remote tarball URL. Fetched
    /// once at resolve time so the resolver can read the enclosed
    /// `package.json` for version + transitive deps and pin the
    /// sha512 integrity. `integrity` stays empty on freshly-parsed
    /// specifiers and is filled in by the resolver after download.
    RemoteTarball(RemoteTarballSource),
}

/// A remote tarball dependency spec. See [`LocalSource::RemoteTarball`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarballSource {
    pub url: String,
    pub integrity: String,
}

/// A git dependency spec. See [`LocalSource::Git`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub url: String,
    pub committish: Option<String>,
    pub resolved: String,
    /// pnpm `&path:/sub/dir` selector — when set, only this
    /// subdirectory of the cloned repo is treated as the package
    /// root. Stored without leading slash so dep_path hashes are
    /// stable regardless of whether the user wrote `path:/x` or
    /// `path:x`.
    pub subpath: Option<String>,
}

impl LocalSource {
    /// The original path (relative to the project root) the user wrote
    /// in `package.json`. `None` for non-path sources like git.
    pub fn path(&self) -> Option<&Path> {
        match self {
            LocalSource::Directory(p) | LocalSource::Tarball(p) | LocalSource::Link(p) => Some(p),
            LocalSource::Git(_) | LocalSource::RemoteTarball(_) => None,
        }
    }

    /// The protocol kind (`"file"` / `"link"` / `"git"` / `"url"`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            LocalSource::Directory(_) | LocalSource::Tarball(_) => "file",
            LocalSource::Link(_) => "link",
            LocalSource::Git(_) => "git",
            LocalSource::RemoteTarball(_) => "url",
        }
    }

    /// The path as a POSIX-style string with forward-slash separators.
    /// `Path::display()` and `to_string_lossy()` honor the host's
    /// separator (backslash on Windows), which would make `dep_path`
    /// hashes and lockfile `specifier:` strings non-portable: the
    /// same `file:./some/dir` would render as `some\dir` on Windows
    /// and `some/dir` on Unix, producing two different hashes for
    /// the same logical target. Always rendering with `/` keeps
    /// lockfiles cross-platform identical.
    pub fn path_posix(&self) -> String {
        self.path()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }

    /// Canonical specifier string as pnpm writes it in the `packages:`
    /// and `snapshots:` keys (post-`<name>@` part). For `file:` /
    /// `link:` this is `file:./vendor/foo` / `link:../sibling`. For
    /// `git`, pnpm uses the resolved form `<url>#<commit>` (no
    /// `git+` prefix) because the lockfile pins to the exact commit
    /// regardless of what the user wrote. Always emits POSIX
    /// separators so the resulting lockfile is portable.
    pub fn specifier(&self) -> String {
        match self {
            LocalSource::Git(g) => match &g.subpath {
                Some(sub) => format!("{}#{}&path:/{}", g.url, g.resolved, sub),
                None => format!("{}#{}", g.url, g.resolved),
            },
            LocalSource::RemoteTarball(t) => t.url.clone(),
            _ => format!("{}:{}", self.kind_str(), self.path_posix()),
        }
    }

    /// Internal FS-safe dep_path used as the key in
    /// `LockfileGraph.packages` and as the `.aube/` subdir name.
    ///
    /// Distinct paths must map to distinct keys (otherwise the
    /// linker would silently mix files between two local packages),
    /// and the result must be a single filesystem component — no
    /// `/`, `\`, `:`, or `..`. Ad-hoc character substitution trips
    /// over cases like `../vendor` vs `__/vendor` or `a.b` vs `a_b`
    /// collapsing to the same string, so we hash the raw path bytes
    /// and suffix the first 16 hex chars (64 bits — more than enough
    /// to avoid collisions inside a single project).
    ///
    /// The hash input is the POSIX-form path string so a checked-in
    /// lockfile resolves to the same key regardless of which
    /// platform ran `aube install`.
    pub fn dep_path(&self, name: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        match self {
            LocalSource::Git(g) => {
                hasher.update(g.url.as_bytes());
                hasher.update(b"#");
                hasher.update(g.resolved.as_bytes());
                if let Some(sub) = &g.subpath {
                    hasher.update(b"&path:/");
                    hasher.update(sub.as_bytes());
                }
            }
            LocalSource::RemoteTarball(t) => {
                hasher.update(t.url.as_bytes());
            }
            _ => hasher.update(self.path_posix().as_bytes()),
        }
        let digest = hasher.finalize();
        let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("{name}@{}+{short}", self.kind_str())
    }

    /// Classify a user-written `file:` / `link:` specifier against the
    /// project root. Returns `None` if `spec` isn't a local specifier.
    /// Resolves the target path relative to `project_root`; a `file:`
    /// target that resolves to a `.tgz` / `.tar.gz` on disk is treated
    /// as a tarball, anything else as a directory.
    pub fn parse(spec: &str, project_root: &Path) -> Option<Self> {
        // Check git first so URLs like `https://host/user/repo.git`
        // aren't swallowed by the broader bare-http tarball check
        // below.
        if let Some((url, committish, subpath)) = parse_git_spec(spec) {
            // `resolved` is filled in by the resolver after running
            // `git ls-remote`. A lockfile round-trip that never
            // re-resolves will leave this empty, which is the sentinel
            // the resolver checks for before calling ls-remote.
            return Some(LocalSource::Git(GitSource {
                url,
                committish,
                resolved: String::new(),
                subpath,
            }));
        }
        // Any remaining bare `http(s)://` URL is a remote tarball.
        // npm semantics treat *all* non-git HTTP URLs in a dependency
        // value as tarball URLs, so services that serve tarballs from
        // URLs without a `.tgz` extension (pkg.pr.new, GitHub
        // codeload, etc.) classify correctly here.
        if Self::looks_like_remote_tarball_url(spec) {
            return Some(LocalSource::RemoteTarball(RemoteTarballSource {
                url: spec.to_string(),
                integrity: String::new(),
            }));
        }
        let (kind, rest) = if let Some(r) = spec.strip_prefix("file:") {
            ("file", r)
        } else if let Some(r) = spec.strip_prefix("link:") {
            ("link", r)
        } else {
            return None;
        };
        let rel = PathBuf::from(rest);
        let abs = project_root.join(&rel);
        if kind == "link" {
            return Some(LocalSource::Link(rel));
        }
        if abs.is_file() && Self::path_looks_like_tarball(&rel) {
            return Some(LocalSource::Tarball(rel));
        }
        Some(LocalSource::Directory(rel))
    }

    /// Whether a specifier looks like a direct HTTP(S) URL that should
    /// be fetched as a tarball. Per npm semantics, *any* `http://` or
    /// `https://` URL in a dependency value is a tarball URL — services
    /// like pkg.pr.new, GitHub codeload, and private registries with
    /// auth-token query strings serve tarballs from URLs that don't
    /// carry a `.tgz` extension. Git URLs must already have been
    /// ruled out by the caller (see [`parse_git_spec`]) so a
    /// `.git`-suffixed URL doesn't get misclassified here.
    pub fn looks_like_remote_tarball_url(spec: &str) -> bool {
        spec.starts_with("https://") || spec.starts_with("http://")
    }

    pub fn path_looks_like_tarball(path: &Path) -> bool {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        let lower = name.to_ascii_lowercase();
        lower.ends_with(".tgz") || lower.ends_with(".tar.gz")
    }
}

/// Parse a git dependency specifier into `(clone_url, committish)`.
///
/// Recognized forms:
/// - `git+https://host/user/repo.git[#ref]`
/// - `git+ssh://git@host/user/repo.git[#ref]`
/// - `git://host/user/repo.git[#ref]`
/// - `https://host/user/repo.git[#ref]` (only when ending in `.git`)
/// - `github:user/repo[#ref]` → `https://github.com/user/repo.git`
/// - `gitlab:user/repo[#ref]` → `https://gitlab.com/user/repo.git`
/// - `bitbucket:user/repo[#ref]` → `https://bitbucket.org/user/repo.git`
///
/// Returns `None` for any specifier that doesn't look like a git URL,
/// so the caller can fall through to other protocol parsers.
pub fn parse_git_spec(spec: &str) -> Option<(String, Option<String>, Option<String>)> {
    let (body, committish, subpath) = match spec.find('#') {
        Some(idx) => {
            let (c, s) = parse_git_fragment(&spec[idx + 1..]);
            (&spec[..idx], c, s)
        }
        None => (spec, None, None),
    };
    let is_bare_transport = body.starts_with("https://")
        || body.starts_with("http://")
        || body.starts_with("ssh://")
        || body.starts_with("file://");
    let url = if let Some(rest) = body.strip_prefix("git+") {
        // `git+` explicitly tags the URL as git, so the `.git`
        // suffix is optional (GitHub/GitLab accept both forms).
        rest.to_string()
    } else if body.starts_with("git://") {
        body.to_string()
    } else if let Some(path) = body.strip_prefix("github:") {
        format!("https://github.com/{path}.git")
    } else if let Some(path) = body.strip_prefix("gitlab:") {
        format!("https://gitlab.com/{path}.git")
    } else if let Some(path) = body.strip_prefix("bitbucket:") {
        format!("https://bitbucket.org/{path}.git")
    } else if is_bare_transport && body.ends_with(".git") {
        body.to_string()
    } else if is_bare_transport
        && committish
            .as_deref()
            .is_some_and(|c| c.len() == 40 && c.chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        // Lockfile round-trip form: `specifier()` writes the stored
        // URL verbatim plus `#<sha>`. URLs that dropped the `git+`
        // prefix (and happen to lack `.git`) are disambiguated from
        // plain tarball URLs by the 40-hex committish suffix.
        body.to_string()
    } else {
        return None;
    };
    Some((url, committish, subpath))
}

/// Normalize git URL fragments used by npm-compatible lockfiles.
///
/// Plain git accepts `#<ref>`, while npm and Yarn Berry also write
/// key/value fragments such as `#commit=<sha>` for pinned git deps.
/// Downstream code passes this value directly to `git ls-remote` and
/// `git checkout`, so strip the selector key here and keep only the
/// actual ref name or SHA.
pub(crate) fn normalize_git_fragment(fragment: &str) -> Option<String> {
    parse_git_fragment(fragment).0
}

/// Parse a git URL fragment into `(committish, subpath)`. Handles the
/// pnpm/hosted-git-info form `<ref>&path:/sub/dir` (the `path:` key
/// uses a colon, not `=`, by historical convention) as well as the
/// `key=value` form npm/Yarn Berry write. Unknown selectors are
/// ignored. Subpath is returned without leading slash so the caller
/// can join it with a clone dir without tripping the absolute-path
/// branch of `Path::join`.
pub(crate) fn parse_git_fragment(fragment: &str) -> (Option<String>, Option<String>) {
    if fragment.is_empty() {
        return (None, None);
    }

    let mut fallback: Option<&str> = None;
    let mut preferred: Option<&str> = None;
    let mut subpath: Option<String> = None;
    for part in fragment.split('&') {
        if part.is_empty() {
            continue;
        }
        // Try `key=value` first; fall back to `key:value` only for
        // the small set of well-known selectors so a tag name that
        // happens to contain a colon (e.g. `release:2026-01`) is not
        // misparsed as a key.
        let split = part.split_once('=').or_else(|| {
            part.split_once(':').filter(|(k, _)| {
                matches!(*k, "commit" | "tag" | "head" | "branch" | "path" | "semver")
            })
        });
        let (key, value) = split.unwrap_or(("", part));
        if value.is_empty() {
            continue;
        }
        match key {
            "commit" => preferred = Some(value),
            "tag" | "head" | "branch" => {
                fallback.get_or_insert(value);
            }
            "path" => {
                let trimmed = value.trim_start_matches('/');
                if !trimmed.is_empty() {
                    subpath = Some(trimmed.to_string());
                }
            }
            "" => {
                fallback.get_or_insert(value);
            }
            _ => {}
        }
    }

    (preferred.or(fallback).map(ToString::to_string), subpath)
}

/// A single resolved package in the lockfile.
///
/// The `dependencies` map keys are dep names and values are the dependency's
/// dep_path *tail* — i.e. the string that follows `<name>@`. For a plain
/// package this is just the version (`"4.17.21"`); for a package with its
/// own peer context it includes the suffix (`"18.2.0(prop-types@15.8.1)"`).
/// Combining the key with its value reproduces the full dep_path (which is
/// also the key in `LockfileGraph.packages`).
#[derive(Debug, Clone, Default)]
pub struct LockedPackage {
    /// Package name (e.g., "lodash")
    pub name: String,
    /// Exact resolved version (e.g., "4.17.21")
    pub version: String,
    /// Integrity hash (e.g., "sha512-...")
    pub integrity: Option<String>,
    /// Dependencies of this package (name -> dep_path tail, see struct docs)
    pub dependencies: BTreeMap<String, String>,
    /// Optional dependency edges for this package. Active optional edges are
    /// also mirrored in `dependencies` so graph walks and the linker continue
    /// to see them; this separate map lets platform filtering prune optional
    /// edges without touching regular dependencies.
    pub optional_dependencies: BTreeMap<String, String>,
    /// Peer dependency ranges as *declared* by the package (from its
    /// package.json / packument). These are the constraints; the resolved
    /// versions live in `dependencies` after the peer-context pass runs.
    pub peer_dependencies: BTreeMap<String, String>,
    /// `peerDependenciesMeta` entries, keyed by peer name.
    pub peer_dependencies_meta: BTreeMap<String, PeerDepMeta>,
    /// The dep_path key used in the lockfile. For packages with resolved
    /// peer contexts this includes the suffix, e.g.
    /// `"styled-components@6.1.0(react@18.2.0)"`.
    pub dep_path: String,
    /// Set for non-registry packages (those installed via `file:` or
    /// `link:` specifiers). `None` for the common case of a package
    /// resolved from an npm registry, where `integrity` is the full
    /// record of where the bits came from.
    pub local_source: Option<LocalSource>,
    /// `os` / `cpu` / `libc` arrays from the package's manifest. Used
    /// by the resolver to filter optional deps that can't run on the
    /// current (or user-overridden) platform. Empty arrays mean no
    /// constraint.
    pub os: PlatformList,
    pub cpu: PlatformList,
    pub libc: PlatformList,
    /// Names declared in the package's own `bundledDependencies`. These
    /// ship inside the parent tarball's `node_modules/`, so the resolver
    /// neither fetches nor recurses into them, and the linker avoids
    /// creating sibling symlinks that would shadow the bundled tree.
    /// An empty Vec means "no bundled deps"; `None` is kept as a
    /// distinct value only inside the resolver and collapsed to empty
    /// here because the lockfile round-trip doesn't need to preserve
    /// the "unset" vs "empty list" distinction.
    pub bundled_dependencies: Vec<String>,
    /// Full registry tarball URL for registry-sourced packages. Only
    /// populated when `LockfileSettings::lockfile_include_tarball_url`
    /// is active on this graph; otherwise `None` and the lockfile
    /// writer derives the URL at fetch time from the configured
    /// registry. `local_source`-backed packages (file:, link:, git:,
    /// remote tarball) already carry their own URL via `LocalSource`
    /// and don't populate this field.
    pub tarball_url: Option<String>,
    /// For npm-alias deps (`"h3-v2": "npm:h3@2.0.1-rc.20"`): the real
    /// package name on the registry (`"h3"`). `None` means the entry
    /// is not aliased and `name` already holds the registry name.
    ///
    /// Install semantics when `Some(real)`:
    /// - `name` is the *alias* — that's the folder under `node_modules/`,
    ///   the symlink name for transitive deps, and the key every package
    ///   that declares this dep refers to.
    /// - `alias_of` is the real package name used for tarball URL lookup,
    ///   store index keying, and packument fetches.
    /// - `version` is the real resolved version.
    ///
    /// `registry_name()` returns the right name for registry IO; every
    /// call site that talks to the registry or the CAS uses that helper.
    pub alias_of: Option<String>,
    /// Yarn berry's `checksum:` field, preserved verbatim when parsing a
    /// yarn 2+ lockfile (e.g. `"10c0/<blake2b-hex>"`). The format is
    /// yarn-specific — it uses a yarn-chosen hash family prefixed with
    /// the `cacheKey` that produced it — and doesn't share a hash
    /// algorithm with `integrity` (sha-512). When re-emitting a yarn
    /// berry lockfile we write this field back as-is; packages that
    /// didn't come through a berry parse (e.g. freshly-resolved entries
    /// in a new install) leave this `None` and the writer omits the
    /// `checksum:` field, which berry tolerates at the default
    /// `checksumBehavior: throw` when the cache is fresh.
    pub yarn_checksum: Option<String>,
    /// `engines:` from the package's manifest, round-tripped through
    /// the lockfile so pnpm-style writers can emit the same flow-form
    /// `engines: {node: '>=8'}` line pnpm writes. Empty map means
    /// "no engines declared" — the writer skips the field entirely.
    pub engines: BTreeMap<String, String>,
    /// `bin:` map from the package's manifest, normalized to
    /// `name → path`. An empty map means "no bins declared".
    ///
    /// pnpm-style writers derive `hasBin: true` from
    /// `!bin.is_empty()` (they don't preserve the names/paths); bun's
    /// format emits the full map on the package's meta block. Keeping
    /// the map here lets both writers render byte-identical output
    /// without an extra tarball-level re-parse.
    pub bin: BTreeMap<String, String>,
    /// Dependency ranges as declared in this package's own
    /// `package.json` — keyed by dep name, values are the raw
    /// specifiers (`"^4.1.0"`, `"~1.1.4"`, `"workspace:*"`, …).
    ///
    /// Distinct from [`Self::dependencies`], which stores the
    /// *resolved* dep_path tail (`"4.3.0"`). npm / yarn / bun
    /// lockfiles preserve the declared ranges on every nested
    /// package entry — rewriting them to the resolved pins is the
    /// biggest source of round-trip churn against those formats. This
    /// map lets writers emit the declared range when available and
    /// fall back to the resolved pin otherwise (e.g. when the source
    /// lockfile was pnpm, whose `snapshots:` only carries pins).
    ///
    /// Empty means "unknown" — writers should fall back to pins.
    /// Covers production *and* optional dependencies in one map since
    /// a package can't declare the same name twice across those
    /// sections.
    pub declared_dependencies: BTreeMap<String, String>,
    /// Package's `license` field, collapsed to the simple string
    /// form. Round-tripped so npm's lockfile keeps its per-entry
    /// `"license": "MIT"` line; pnpm / yarn / bun don't record
    /// licenses and leave this `None` on parse.
    pub license: Option<String>,
    /// Package's funding URL, extracted from whatever shape the
    /// manifest's `funding:` field took (string / object / array).
    /// Round-tripped so npm's lockfile keeps its per-entry
    /// `"funding": {"url": "…"}` block.
    pub funding_url: Option<String>,
    /// Per-package-meta extras preserved verbatim from the source
    /// lockfile. Captures fields the typed model doesn't yet cover
    /// (`deprecated`, `hasInstallScript`, bun's `optionalPeers`, and
    /// anything a future lockfile bump adds) so a parse/write cycle
    /// doesn't drop them. Each format's writer re-emits what makes
    /// sense there — bun inlines the extras back on the package-entry
    /// meta object, pnpm / yarn / npm currently ignore them.
    pub extra_meta: BTreeMap<String, serde_json::Value>,
}

impl LockedPackage {
    /// The package name to use for registry / store operations — the real
    /// name behind an npm-alias when aliased, otherwise just `name`. Used
    /// at every site that derives a tarball URL, a packument URL, or an
    /// aube-store cache key so aliased entries hit the actual package
    /// instead of the alias-qualified name.
    pub fn registry_name(&self) -> &str {
        self.alias_of.as_deref().unwrap_or(&self.name)
    }

    /// Canonical `"name@version"` key used as a handle in patches,
    /// approve-builds prompts, lockfile canonical maps, and display
    /// paths. Not the dep-path — that includes peer-context suffixes.
    pub fn spec_key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// Metadata about a single declared peer dependency. Matches the shape of
/// `peerDependenciesMeta` in package.json.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerDepMeta {
    /// When true, an unmet peer is silently allowed rather than warned about.
    pub optional: bool,
}

/// Which source lockfile format was parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileKind {
    /// `aube-lock.yaml` — aube's default lockfile when no existing
    /// lockfile is present. Same on-disk format as pnpm v9 for now
    /// (we piggyback on pnpm::read/write).
    Aube,
    /// `pnpm-lock.yaml` — pnpm v9 format. If this is the existing
    /// project lockfile, aube reads and writes it in place.
    Pnpm,
    Npm,
    /// `yarn.lock` v1 (classic yarn). Line-based text format with
    /// 2-space indented fields.
    Yarn,
    /// `yarn.lock` v2+ (yarn berry). YAML format with `__metadata:`
    /// header, `resolution:` / `checksum:` fields, and
    /// `languageName` / `linkType`. Same filename as `Yarn`; detection
    /// peeks at the content for the `__metadata:` marker to pick
    /// between the two.
    YarnBerry,
    NpmShrinkwrap,
    Bun,
}

impl LockfileKind {
    pub fn filename(self) -> &'static str {
        match self {
            LockfileKind::Aube => "aube-lock.yaml",
            LockfileKind::Pnpm => "pnpm-lock.yaml",
            LockfileKind::Npm => "package-lock.json",
            LockfileKind::Yarn | LockfileKind::YarnBerry => "yarn.lock",
            LockfileKind::NpmShrinkwrap => "npm-shrinkwrap.json",
            LockfileKind::Bun => "bun.lock",
        }
    }
}

impl LockfileGraph {
    /// Get all direct dependencies of the root project.
    pub fn root_deps(&self) -> &[DirectDep] {
        self.importers.get(".").map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Get a package by its dep_path key.
    pub fn get_package(&self, dep_path: &str) -> Option<&LockedPackage> {
        self.packages.get(dep_path)
    }

    /// `true` when at least one package in the graph carries a
    /// non-empty `bin` map — a cheap signal that the source (lockfile
    /// parser or fresh resolve) populated bin metadata. Bin-linking
    /// passes use this to short-circuit the `package.json` read on
    /// packages whose `bin` is empty (95%+ of a typical graph).
    ///
    /// The pnpm, bun, npm, and aube parsers all fill `bin`; a fresh
    /// resolve fills it from packument data. The yarn-classic parser
    /// leaves it empty, so a graph loaded exclusively from `yarn.lock`
    /// returns `false` here and bin linking falls back to the full
    /// `package.json` read. That's a correctness-over-speed choice:
    /// misreading "empty" as "no bin" on yarn would silently drop
    /// executables from `node_modules/.bin/`.
    pub fn has_bin_metadata(&self) -> bool {
        self.packages.values().any(|p| !p.bin.is_empty())
    }

    /// BFS the transitive closure of `roots` through `self.packages`,
    /// returning every reachable dep_path (roots included). Missing
    /// roots are skipped silently — a root without a matching package
    /// is treated as a leaf, which matches what `filter_deps` /
    /// `subset_to_importer` need when a retained importer points at a
    /// package that was never fully installed (e.g. optional deps
    /// filtered out on this platform).
    ///
    /// `LockedPackage.dependencies` maps `child_name → dep_path tail`,
    /// so each child's full key reconstructs as `{child_name}@{tail}`.
    fn transitive_closure<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a str>,
    ) -> std::collections::HashSet<String> {
        let mut reachable: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for root in roots {
            if reachable.insert(root.to_string()) {
                queue.push_back(root.to_string());
            }
        }
        while let Some(dep_path) = queue.pop_front() {
            let Some(pkg) = self.packages.get(&dep_path) else {
                continue;
            };
            for (child_name, child_version) in &pkg.dependencies {
                let child_key = format!("{child_name}@{child_version}");
                if reachable.insert(child_key.clone()) {
                    queue.push_back(child_key);
                }
            }
        }
        reachable
    }

    /// Clone only the `packages` entries whose keys are in `reachable`.
    /// Paired with `transitive_closure` to produce the pruned
    /// `LockfileGraph.packages` for `filter_deps` / `subset_to_importer`.
    fn packages_restricted_to(
        &self,
        reachable: &std::collections::HashSet<String>,
    ) -> BTreeMap<String, LockedPackage> {
        self.packages
            .iter()
            .filter(|(dep_path, _)| reachable.contains(*dep_path))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Produce a new `LockfileGraph` containing only the direct deps that match
    /// `keep` and the transitive deps reachable from them.
    ///
    /// Used by `install --prod` to drop `DepType::Dev` roots and everything
    /// only reachable through them, and by `install --no-optional` for optional
    /// deps. The filter runs over every importer's direct-dep list, so workspace
    /// projects behave correctly.
    ///
    /// Packages that are reachable from a retained root through a transitive
    /// chain are kept even if a pruned dev dep also happened to depend on them —
    /// the check is "is this package reachable from any retained root?", not
    /// "was this package introduced by a retained root?".
    pub fn filter_deps<F>(&self, keep: F) -> LockfileGraph
    where
        F: Fn(&DirectDep) -> bool,
    {
        // Filter each importer's DirectDep list.
        let importers: BTreeMap<String, Vec<DirectDep>> = self
            .importers
            .iter()
            .map(|(path, deps)| {
                let filtered: Vec<DirectDep> = deps.iter().filter(|d| keep(d)).cloned().collect();
                (path.clone(), filtered)
            })
            .collect();

        // BFS from every retained root across every importer.
        let reachable = self.transitive_closure(
            importers
                .values()
                .flat_map(|deps| deps.iter().map(|d| d.dep_path.as_str())),
        );
        let packages = self.packages_restricted_to(&reachable);

        LockfileGraph {
            importers,
            packages,
            // Preserve the source graph's settings — filter is a
            // structural operation, not a resolution-mode reset.
            // Writing the filtered graph (e.g. from `aube prune`) must
            // emit the same `settings:` header the user chose.
            settings: self.settings.clone(),
            // Overrides are part of the user's resolution intent and
            // should survive structural filters like `aube prune`.
            overrides: self.overrides.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            // Times follow the same round-trip invariant as settings:
            // filter doesn't change what versions are locked, so the
            // per-package publish timestamps carry through unchanged.
            times: self.times.clone(),
            skipped_optional_dependencies: self.skipped_optional_dependencies.clone(),
            catalogs: self.catalogs.clone(),
            bun_config_version: self.bun_config_version,
            patched_dependencies: self.patched_dependencies.clone(),
            trusted_dependencies: self.trusted_dependencies.clone(),
            extra_fields: self.extra_fields.clone(),
            workspace_extra_fields: self.workspace_extra_fields.clone(),
        }
    }

    /// Produce a new `LockfileGraph` rooted at the importer at
    /// `importer_path`, with its transitive closure preserved and every
    /// other importer dropped. The retained importer is remapped to
    /// `"."` because the consumer installs the result as a standalone
    /// project.
    ///
    /// Used by `aube deploy`: reading the source workspace lockfile
    /// and subsetting it to the deployed package lets a frozen install
    /// in the target reproduce the workspace's exact versions without
    /// re-resolving against the registry. `keep` filters the importer's
    /// direct deps the same way `filter_deps` does, so `--prod` /
    /// `--dev` / `--no-optional` deploys drop the matching roots.
    ///
    /// Returns `None` if `importer_path` is not present in
    /// `self.importers`. Graph-wide metadata (`settings`, `overrides`,
    /// `times`, `catalogs`, `ignored_optional_dependencies`) is copied
    /// verbatim — structural pruning, not a resolution-mode reset.
    /// Callers targeting a non-workspace install may want to clear
    /// workspace-scope fields that would otherwise trigger drift
    /// detection against a rewritten target manifest.
    pub fn subset_to_importer<F>(&self, importer_path: &str, keep: F) -> Option<LockfileGraph>
    where
        F: Fn(&DirectDep) -> bool,
    {
        let src_deps = self.importers.get(importer_path)?;
        let kept: Vec<DirectDep> = src_deps.iter().filter(|d| keep(d)).cloned().collect();

        // BFS the transitive closure from retained roots, scoped to
        // just this importer's kept direct deps.
        let reachable = self.transitive_closure(kept.iter().map(|d| d.dep_path.as_str()));
        let packages = self.packages_restricted_to(&reachable);

        // Per-importer metadata: keep only the retained importer's
        // entry, rekeyed to `.`. The source workspace's other
        // importers are meaningless in a target that has exactly one.
        let mut skipped_optional_dependencies = BTreeMap::new();
        if let Some(skipped) = self.skipped_optional_dependencies.get(importer_path) {
            skipped_optional_dependencies.insert(".".to_string(), skipped.clone());
        }

        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), kept);

        Some(LockfileGraph {
            importers,
            packages,
            settings: self.settings.clone(),
            overrides: self.overrides.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            times: self.times.clone(),
            skipped_optional_dependencies,
            catalogs: self.catalogs.clone(),
            bun_config_version: self.bun_config_version,
            patched_dependencies: self.patched_dependencies.clone(),
            trusted_dependencies: self.trusted_dependencies.clone(),
            extra_fields: self.extra_fields.clone(),
            workspace_extra_fields: self.workspace_extra_fields.clone(),
        })
    }

    /// Overlay per-package metadata fields from `prior` onto `self`
    /// for every `(name, version)` that survives in both graphs.
    /// Carries forward only fields the abbreviated packument (npm
    /// corgi) doesn't ship — `license`, `funding_url`, and the
    /// bun-format `configVersion` — so a fresh re-resolve against
    /// the same spec set doesn't lose them.
    ///
    /// Keyed by canonical `name@version`, so a peer-context rewrite
    /// between the old and new graph still lines up. `self`'s own
    /// values win when set (fresh registry data is authoritative);
    /// `prior`'s fill in only the `None` / empty slots. Safe to call
    /// on any pair of graphs — parsing the old lockfile is the
    /// caller's concern.
    pub fn overlay_metadata_from(&mut self, prior: &LockfileGraph) {
        // Build a canonical `name@version → prior pkg` lookup once so
        // repeated peer-context variants in `self.packages` all hit
        // the same prior entry.
        let prior_index = build_canonical_map(prior);
        for pkg in self.packages.values_mut() {
            let key = pkg.spec_key();
            let Some(prior_pkg) = prior_index.get(&key) else {
                continue;
            };
            if pkg.license.is_none() && prior_pkg.license.is_some() {
                pkg.license = prior_pkg.license.clone();
            }
            if pkg.funding_url.is_none() && prior_pkg.funding_url.is_some() {
                pkg.funding_url = prior_pkg.funding_url.clone();
            }
            // Per-entry extras (`deprecated`, `optionalPeers`,
            // format-specific fields bun/npm/yarn wrote into the
            // meta block) can't be recovered from a fresh resolve,
            // so carry them forward when the newer graph doesn't
            // already carry its own. `self`-side keys always win.
            for (k, v) in &prior_pkg.extra_meta {
                pkg.extra_meta.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if self.bun_config_version.is_none() {
            self.bun_config_version = prior.bun_config_version;
        }
        if self.patched_dependencies.is_empty() {
            self.patched_dependencies = prior.patched_dependencies.clone();
        }
        if self.trusted_dependencies.is_empty() {
            self.trusted_dependencies = prior.trusted_dependencies.clone();
        }
        if self.extra_fields.is_empty() {
            self.extra_fields = prior.extra_fields.clone();
        }
        if self.workspace_extra_fields.is_empty() {
            self.workspace_extra_fields = prior.workspace_extra_fields.clone();
        }
    }

    /// Compare this lockfile's root importer against a single manifest.
    ///
    /// Mirrors pnpm's `prefer-frozen-lockfile` check: a lockfile is "fresh" iff
    /// every direct dep specifier in `package.json` exactly matches the specifier
    /// recorded in the lockfile (string compare, not semver). Used to decide
    /// whether to skip resolution and trust the lockfile (`Fresh`) or fall back
    /// to a full re-resolve (`Stale { reason }`).
    ///
    /// For workspace projects, use [`check_drift_workspace`] instead — this
    /// method only inspects the root importer.
    ///
    /// `workspace_overrides` is the `overrides:` block from
    /// `pnpm-workspace.yaml` (pnpm v10 moved overrides there). Pass an
    /// empty map when the project has no workspace-yaml overrides. Keys
    /// are merged on top of `manifest.overrides_map()` before the drift
    /// comparison, matching the resolver's effective-override set —
    /// otherwise a lockfile written with a workspace override
    /// immediately looks stale on the next `--frozen-lockfile` run.
    ///
    /// `workspace_ignored_optional` is the same idea for
    /// `pnpm-workspace.yaml`'s `ignoredOptionalDependencies` block:
    /// the resolver unions it with the manifest's list, so the drift
    /// check has to see the same union or a freshly-written lockfile
    /// immediately reads as stale.
    ///
    /// `workspace_catalogs` is the `catalog:` / `catalogs:` block from
    /// `pnpm-workspace.yaml`. pnpm resolves `catalog:` references in
    /// override values against this map before writing the lockfile
    /// and before comparing on re-install, so both sides of the drift
    /// check have to see the catalog-resolved form — otherwise a
    /// `"lodash": "catalog:"` override reads as stale against a
    /// lockfile that recorded the resolved `"lodash": "4.17.21"`.
    ///
    /// Lockfile formats that don't record specifiers (npm, yarn, bun) always
    /// return `Fresh` since we have no way to detect drift without re-resolving.
    ///
    /// [`check_drift_workspace`]: Self::check_drift_workspace
    pub fn check_drift(
        &self,
        manifest: &aube_manifest::PackageJson,
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> DriftStatus {
        let effective = resolve_catalog_refs_in_overrides(
            &merge_manifest_and_workspace_overrides(manifest, workspace_overrides),
            workspace_catalogs,
        );
        let locked = resolve_catalog_refs_in_overrides(&self.overrides, workspace_catalogs);
        if let Some(reason) = overrides_drift_reason(&locked, &effective) {
            return DriftStatus::Stale { reason };
        }
        let mut effective_ignored = manifest.pnpm_ignored_optional_dependencies();
        effective_ignored.extend(workspace_ignored_optional.iter().cloned());
        if let Some(reason) =
            ignored_optional_drift_reason(&self.ignored_optional_dependencies, &effective_ignored)
        {
            return DriftStatus::Stale { reason };
        }
        self.check_drift_for_importer(".", manifest, &effective)
    }

    /// Workspace-aware drift check.
    ///
    /// Each entry in `manifests` is `(importer_path, manifest)` — for example
    /// `(".", root_manifest), ("packages/app", app_manifest), ...`. Every
    /// importer is checked against its own manifest; the first stale importer
    /// determines the result.
    ///
    /// See [`check_drift`] for the `workspace_overrides` contract.
    ///
    /// [`check_drift`]: Self::check_drift
    pub fn check_drift_workspace(
        &self,
        manifests: &[(String, aube_manifest::PackageJson)],
        workspace_overrides: &BTreeMap<String, String>,
        workspace_ignored_optional: &[String],
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> DriftStatus {
        // Override drift is checked once at the workspace level, against
        // the root manifest. Workspace-package manifests may declare
        // their own `overrides` blocks but pnpm only honors the root's,
        // so we mirror that here.
        let effective_overrides = match manifests.iter().find(|(p, _)| p == ".") {
            Some((_, root_manifest)) => {
                let effective = resolve_catalog_refs_in_overrides(
                    &merge_manifest_and_workspace_overrides(root_manifest, workspace_overrides),
                    workspace_catalogs,
                );
                let locked = resolve_catalog_refs_in_overrides(&self.overrides, workspace_catalogs);
                if let Some(reason) = overrides_drift_reason(&locked, &effective) {
                    return DriftStatus::Stale { reason };
                }
                let mut effective_ignored = root_manifest.pnpm_ignored_optional_dependencies();
                effective_ignored.extend(workspace_ignored_optional.iter().cloned());
                if let Some(reason) = ignored_optional_drift_reason(
                    &self.ignored_optional_dependencies,
                    &effective_ignored,
                ) {
                    return DriftStatus::Stale { reason };
                }
                effective
            }
            None => BTreeMap::new(),
        };
        for (importer_path, manifest) in manifests {
            match self.check_drift_for_importer(importer_path, manifest, &effective_overrides) {
                DriftStatus::Fresh => continue,
                stale => return stale,
            }
        }
        DriftStatus::Fresh
    }

    /// Compare this lockfile's catalog snapshot against the current
    /// `pnpm-workspace.yaml` catalogs.
    ///
    /// pnpm only writes catalog entries that at least one importer
    /// references — unused entries are absent from the lockfile. So
    /// "missing from lockfile" doesn't mean "added by the user", it
    /// means "declared but unreferenced", which is not drift. The
    /// transition from unused → used is caught by the importer-level
    /// drift check, since a fresh `catalog:` reference shows up as a
    /// new dep in some `package.json`.
    ///
    /// We fire on two cases only:
    /// - the spec changed for an entry the lockfile already records
    ///   (the entry is in use, and re-resolution must rerun);
    /// - the workspace removed an entry that the lockfile records
    ///   (the importer using `catalog:` now points at nothing).
    ///
    /// Resolved versions are deliberately not part of the comparison —
    /// the version is an *output* of resolution, so a stale lockfile
    /// version is what re-resolution is supposed to fix. Drift only
    /// fires on user intent (the specifier).
    pub fn check_catalogs_drift(
        &self,
        workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> DriftStatus {
        for (cat_name, cat) in workspace_catalogs {
            let Some(locked) = self.catalogs.get(cat_name) else {
                continue;
            };
            for (pkg, spec) in cat {
                if let Some(entry) = locked.get(pkg)
                    && entry.specifier != *spec
                {
                    return DriftStatus::Stale {
                        reason: format!(
                            "catalogs.{cat_name}.{pkg}: workspace says {spec}, lockfile says {}",
                            entry.specifier
                        ),
                    };
                }
            }
        }
        for (cat_name, cat) in &self.catalogs {
            let workspace_cat = workspace_catalogs.get(cat_name);
            for pkg in cat.keys() {
                if workspace_cat.map(|c| c.contains_key(pkg)) != Some(true) {
                    return DriftStatus::Stale {
                        reason: format!("catalogs.{cat_name}: workspace removed {pkg}"),
                    };
                }
            }
        }
        DriftStatus::Fresh
    }

    /// Compare a single importer's `DirectDep` list against the corresponding
    /// `package.json`. Used by both [`check_drift`] and [`check_drift_workspace`].
    ///
    /// [`check_drift`]: Self::check_drift
    /// [`check_drift_workspace`]: Self::check_drift_workspace
    fn check_drift_for_importer(
        &self,
        importer_path: &str,
        manifest: &aube_manifest::PackageJson,
        effective_overrides: &BTreeMap<String, String>,
    ) -> DriftStatus {
        let label = if importer_path == "." {
            String::new()
        } else {
            format!("{importer_path}: ")
        };

        let importer_deps: &[DirectDep] = self
            .importers
            .get(importer_path)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // Skip the check entirely if no DirectDep has a specifier (non-pnpm format).
        if importer_deps.iter().all(|d| d.specifier.is_none()) {
            return DriftStatus::Fresh;
        }
        let lockfile_specs: BTreeMap<&str, &str> = importer_deps
            .iter()
            .filter_map(|d| d.specifier.as_deref().map(|s| (d.name.as_str(), s)))
            .collect();

        // Optionals the previous resolve recorded as intentionally
        // skipped on this importer's platform — keyed by name, value
        // is the specifier captured at that time. Distinct from
        // `ignored_optional_dependencies`, which is the user's static
        // ignore list; this map captures *runtime* platform skips.
        let skipped_optionals: BTreeMap<&str, &str> = self
            .skipped_optional_dependencies
            .get(importer_path)
            .map(|m| m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
            .unwrap_or_default();

        // Iterate prod / dev / optional with a flag so the
        // skipped-optional exemption only applies to deps that came
        // from `optional_dependencies`. Without the flag, moving a
        // previously-skipped optional into `dependencies` with the same
        // specifier would silently report Fresh and the dep would
        // never install as a required dep.
        //
        // Optionals named in `ignored_optional_dependencies` are
        // dropped from the manifest-side scan: the resolver never
        // enqueues them, so the lockfile importer never has them
        // either, and the loop would otherwise report drift on every
        // install. (Their *spec* is still verified separately by the
        // round-tripped `ignored_optional_dependencies` block below.)
        let ignored = &self.ignored_optional_dependencies;
        let manifest_deps = manifest
            .dependencies
            .iter()
            .map(|(k, v)| (k, v, false))
            .chain(manifest.dev_dependencies.iter().map(|(k, v)| (k, v, false)))
            .chain(
                manifest
                    .optional_dependencies
                    .iter()
                    .filter(|(name, _)| !ignored.contains(name.as_str()))
                    .map(|(k, v)| (k, v, true)),
            );

        for (name, spec, is_optional) in manifest_deps {
            match lockfile_specs.get(name.as_str()) {
                None => {
                    // A *missing* optional dep is only "fresh" if the
                    // previous resolve recorded it as intentionally
                    // skipped (platform mismatch or
                    // `pnpm.ignoredOptionalDependencies`) AND the
                    // recorded specifier still matches what's in the
                    // manifest. A genuinely *new* optional that the
                    // resolver has never seen is real drift — without
                    // that branch, adding `fsevents` to a fresh manifest
                    // would silently never get installed.
                    if is_optional && let Some(locked_spec) = skipped_optionals.get(name.as_str()) {
                        if *locked_spec == spec {
                            continue;
                        }
                        return DriftStatus::Stale {
                            reason: format!(
                                "{label}{name}: manifest says {spec}, lockfile (skipped) says {locked_spec}"
                            ),
                        };
                    }
                    return DriftStatus::Stale {
                        reason: format!("{label}manifest adds {name}@{spec}"),
                    };
                }
                Some(locked_spec) if *locked_spec != spec => {
                    // pnpm rewrites the importer specifier to the
                    // override-applied value when a bare-name override
                    // fires, so a pnpm-generated lockfile shows
                    // `specifier: 4.17.21` even though `package.json`
                    // still reads `^4.17.0`. Accept that as fresh when
                    // the effective override for this name matches the
                    // lockfile's recorded spec — otherwise any
                    // pnpm-written lockfile with overrides reads stale
                    // on every frozen install.
                    if let Some(override_spec) = effective_overrides.get(name.as_str())
                        && override_spec == locked_spec
                    {
                        continue;
                    }
                    return DriftStatus::Stale {
                        reason: format!(
                            "{label}{name}: manifest says {spec}, lockfile says {locked_spec}"
                        ),
                    };
                }
                Some(_) => {}
            }
        }

        // Anything in the lockfile but missing from the manifest is stale
        // — UNLESS it was auto-hoisted as a peer by the resolver. pnpm-style
        // `auto-install-peers=true` puts peers into the importer's
        // `dependencies` without the user having written them in
        // `package.json`, so we have to recognize those as derived state
        // rather than user intent.
        //
        // Critically, we identify an auto-hoisted entry by matching its
        // *recorded specifier* against peer ranges declared in the graph,
        // not just by name. A name-only check would silently exempt a
        // user-pinned `react` that the user later removed (if any package
        // anywhere in the graph peer-declares react, the name match would
        // fire and we'd report Fresh forever — defeating the drift check).
        //
        // The rule: a lockfile entry whose (name, specifier) pair exactly
        // matches some package's declared (peer_name, peer_range) is
        // auto-hoisted. If the user had pinned react with a different
        // specifier string and then removed it, the (name, specifier)
        // pair no longer matches any peer range, and drift correctly
        // fires so the resolver re-runs and rewrites the lockfile.
        let manifest_names: std::collections::HashSet<&str> = manifest
            .dependencies
            .keys()
            .chain(manifest.dev_dependencies.keys())
            .chain(
                manifest
                    .optional_dependencies
                    .keys()
                    .filter(|name| !ignored.contains(name.as_str())),
            )
            .map(|s| s.as_str())
            .collect();
        let auto_hoisted_peer_specs: std::collections::HashSet<(&str, &str)> = self
            .packages
            .values()
            .flat_map(|p| {
                p.peer_dependencies
                    .iter()
                    .map(|(name, range)| (name.as_str(), range.as_str()))
            })
            .collect();
        for (locked_name, locked_spec) in &lockfile_specs {
            if manifest_names.contains(locked_name) {
                continue;
            }
            if auto_hoisted_peer_specs.contains(&(*locked_name, *locked_spec)) {
                continue;
            }
            return DriftStatus::Stale {
                reason: format!("{label}manifest removed {locked_name}"),
            };
        }

        DriftStatus::Fresh
    }
}

/// Merge `pnpm-workspace.yaml` overrides on top of the manifest's
/// `overrides_map()`. Workspace entries win on key conflict, matching
/// pnpm v10's behavior where the workspace yaml is the canonical
/// home for overrides. Callers pass this into `overrides_drift_reason`
/// so the drift check sees the same effective map the resolver used.
fn merge_manifest_and_workspace_overrides(
    manifest: &aube_manifest::PackageJson,
    workspace_overrides: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = manifest.overrides_map();
    for (k, v) in workspace_overrides {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Rewrite `catalog:` / `catalog:<name>` override values to the catalog's
/// resolved range. pnpm writes resolved override values into the lockfile
/// and compares against the resolved form on re-install, so both sides
/// of the drift check have to see the catalog-substituted map — otherwise
/// a `"lodash": "catalog:"` workspace-yaml override reads as stale against
/// a lockfile that recorded `"lodash": "4.17.21"`. Unresolvable references
/// (missing catalog or missing entry) pass through untouched; the caller
/// would have errored at resolve time if this ever reached a real install,
/// so a drift-mismatch here is fine.
fn resolve_catalog_refs_in_overrides(
    overrides: &BTreeMap<String, String>,
    workspace_catalogs: &BTreeMap<String, BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    overrides
        .iter()
        .map(|(k, v)| {
            let resolved = v
                .strip_prefix("catalog:")
                .map(|tail| if tail.is_empty() { "default" } else { tail })
                .and_then(|cat_name| workspace_catalogs.get(cat_name))
                .and_then(|cat| cat.get(override_key_package_name(k)))
                .cloned()
                .unwrap_or_else(|| v.clone());
            (k.clone(), resolved)
        })
        .collect()
}

/// Extract the package name from an override selector key so the catalog
/// can be looked up by pkg name. Handles bare (`lodash`), scoped
/// (`@babel/core`), ranged (`lodash@<5`), ancestor-chained
/// (`parent>lodash`), and combinations. Unparseable keys return the
/// input unchanged; the catalog lookup will then miss and leave the
/// value as-is.
fn override_key_package_name(key: &str) -> &str {
    let last = key.rsplit('>').next().unwrap_or(key);
    if let Some(after_scope) = last.strip_prefix('@') {
        match after_scope.find('@') {
            Some(idx) => &last[..idx + 1],
            None => last,
        }
    } else {
        match last.find('@') {
            Some(idx) => &last[..idx],
            None => last,
        }
    }
}

/// Compare two override maps and return a human-readable reason
/// describing the first difference, or `None` if they're identical.
/// Drift messages cite the offending key by name so users can act on
/// them — `(lockfile: N entries, manifest: M entries)` is useless
/// when N == M but a value changed.
fn overrides_drift_reason(
    lockfile: &BTreeMap<String, String>,
    manifest: &BTreeMap<String, String>,
) -> Option<String> {
    for (k, v) in manifest {
        match lockfile.get(k) {
            None => return Some(format!("overrides: manifest adds {k}@{v}")),
            Some(locked) if locked != v => {
                return Some(format!("overrides: {k} changed ({locked} → {v})"));
            }
            Some(_) => {}
        }
    }
    for k in lockfile.keys() {
        if !manifest.contains_key(k) {
            return Some(format!("overrides: manifest removes {k}"));
        }
    }
    None
}

/// Compare two `ignoredOptionalDependencies` sets and return a drift
/// reason string for the first difference, or `None` if identical.
fn ignored_optional_drift_reason(
    lockfile: &BTreeSet<String>,
    manifest: &BTreeSet<String>,
) -> Option<String> {
    for name in manifest {
        if !lockfile.contains(name) {
            return Some(format!("ignoredOptionalDependencies: manifest adds {name}"));
        }
    }
    for name in lockfile {
        if !manifest.contains(name) {
            return Some(format!(
                "ignoredOptionalDependencies: manifest removes {name}"
            ));
        }
    }
    None
}

/// Result of comparing a lockfile against a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftStatus {
    /// The lockfile is in sync with the manifest. Safe to use without re-resolving.
    Fresh,
    /// The lockfile is out of date. The reason describes the first mismatch found.
    Stale { reason: String },
}

/// Atomic lockfile write. Tempfile in the same dir, fsync, rename
/// over the target. Every format writer goes through this so a
/// crash or Ctrl+C mid-write cannot leave a truncated lockfile on
/// disk. Rename is atomic on POSIX, on Windows MoveFileEx gives
/// the same guarantee post Win10. Caller passes the serialized
/// bytes already formatted, this just handles the IO layer.
pub(crate) fn atomic_write_lockfile(path: &Path, body: &[u8]) -> Result<(), Error> {
    aube_util::fs_atomic::atomic_write(path, body).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Write a lockfile to the given project directory using aube's default
/// filename (`aube-lock.yaml`, or `aube-lock.<branch>.yaml` when branch
/// lockfiles are enabled).
pub fn write_lockfile(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    write_lockfile_as(project_dir, graph, manifest, LockfileKind::Aube)?;
    Ok(())
}

/// Write a lockfile using the existing project lockfile kind, or
/// Collapse peer-context variants from `graph` into a single map keyed
/// by `"name@version"`, pointing at the first-seen package. Several
/// writers (npm, yarn, …) share this shape: one canonical entry per
/// `(name, version)` pair regardless of how many peer suffixes the
/// full graph emits.
pub fn build_canonical_map(graph: &LockfileGraph) -> BTreeMap<String, &LockedPackage> {
    let mut canonical: BTreeMap<String, &LockedPackage> = BTreeMap::new();
    for pkg in graph.packages.values() {
        canonical.entry(pkg.spec_key()).or_insert(pkg);
    }
    canonical
}

/// `aube-lock.yaml` when the project does not have one yet.
///
/// This is the default write path for commands that mutate the active
/// project graph (`install`, `add`, `remove`, `update`, `dedupe`, ...).
pub fn write_lockfile_preserving_existing(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<PathBuf, Error> {
    let kind = detect_existing_lockfile_kind(project_dir).unwrap_or(LockfileKind::Aube);
    write_lockfile_as(project_dir, graph, manifest, kind)
}

/// Write `graph` in the requested lockfile format into `project_dir`.
///
/// Returns the path that was actually written (useful for logging
/// since `Aube` may resolve to a branch-specific filename). Callers
/// that want to preserve whatever format was already on disk should
/// pair this with [`detect_existing_lockfile_kind`].
///
/// All supported formats: `Aube`, `Pnpm`, `Npm`, `NpmShrinkwrap`,
/// `Yarn`, and `Bun`. This preserves the lockfile kind that already
/// exists in the project; callers should pass `Aube` only when no
/// lockfile exists yet. See each writer module's doc comment for
/// per-format lossy areas (peer contexts, `resolved` URLs, etc.).
pub fn write_lockfile_as(
    project_dir: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    kind: LockfileKind,
) -> Result<PathBuf, Error> {
    let filename = match kind {
        LockfileKind::Aube => aube_lock_filename(project_dir),
        LockfileKind::Pnpm => pnpm_lock_filename(project_dir),
        other => other.filename().to_string(),
    };
    let path = project_dir.join(&filename);
    match kind {
        LockfileKind::Aube | LockfileKind::Pnpm => pnpm::write(&path, graph, manifest)?,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::write(&path, graph, manifest)?,
        LockfileKind::Yarn => yarn::write_classic(&path, graph, manifest)?,
        LockfileKind::YarnBerry => yarn::write_berry(&path, graph, manifest)?,
        LockfileKind::Bun => bun::write(&path, graph, manifest)?,
    }
    Ok(path)
}

/// Return the [`LockfileKind`] of the lockfile already on disk in
/// `project_dir`, if any. Follows the same precedence as
/// [`parse_lockfile_with_kind`] (aube > pnpm > bun > yarn >
/// npm-shrinkwrap > npm). Used by install to preserve a project's
/// existing lockfile format when rewriting after a re-resolve — a
/// user with only `pnpm-lock.yaml`, `package-lock.json`, or another
/// supported lockfile gets that file written back, not a surprise
/// `aube-lock.yaml` alongside it.
pub fn detect_existing_lockfile_kind(project_dir: &Path) -> Option<LockfileKind> {
    for (path, kind) in lockfile_candidates(project_dir, /*include_aube=*/ true) {
        if path.exists() {
            return Some(refine_yarn_kind(&path, kind));
        }
    }
    None
}

/// Resolve the canonical lockfile filename for `project_dir` (aube's own).
///
/// Returns `aube-lock.<branch>.yaml` when `gitBranchLockfile: true` is
/// set in `pnpm-workspace.yaml` (or `aube-workspace.yaml`) and the
/// project is inside a git checkout with a current branch. Forward
/// slashes in the branch name are encoded as `!`, matching pnpm. Falls
/// back to plain `aube-lock.yaml` in every other case.
///
/// Memoized per `project_dir` for the lifetime of the process: a
/// single install resolves this 3–5 times (lockfile_candidates,
/// write_lockfile, debug log, state read/write), and
/// `check_needs_install` runs on every `aube run`/`aube exec` via
/// `ensure_installed`. Without caching, every command would pay for a
/// YAML parse + a `git branch --show-current` subprocess just to
/// recompute a value that can't change mid-process.
pub fn aube_lock_filename(project_dir: &Path) -> String {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(project_dir)
    {
        return hit.clone();
    }
    let resolved = if !git_branch_lockfile_enabled(project_dir) {
        "aube-lock.yaml".to_string()
    } else {
        match current_git_branch(project_dir) {
            Some(branch) => format!("aube-lock.{}.yaml", branch.replace('/', "!")),
            None => "aube-lock.yaml".to_string(),
        }
    };
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), resolved.clone());
    }
    resolved
}

/// Resolve the pnpm lockfile filename for `project_dir`.
///
/// Mirrors [`aube_lock_filename`] for branch lockfiles, but keeps the
/// pnpm filename prefix so projects with an existing `pnpm-lock.yaml`
/// keep writing to pnpm's file.
pub fn pnpm_lock_filename(project_dir: &Path) -> String {
    let aube_name = aube_lock_filename(project_dir);
    // `aube_lock_filename` always returns "aube-lock.<rest>", so strip_prefix
    // always succeeds. The fallback is purely defensive.
    aube_name
        .strip_prefix("aube-lock.")
        .map(|rest| format!("pnpm-lock.{rest}"))
        .unwrap_or_else(|| "pnpm-lock.yaml".to_string())
}

fn git_branch_lockfile_enabled(project_dir: &Path) -> bool {
    // Goes through the build-time-generated typed accessor in
    // `aube_settings::resolved` so the alias list is driven off
    // `settings.toml` — no hand-maintained typed field. This path
    // reads only `pnpm-workspace.yaml`; `.npmrc` values are out of
    // scope here because aube-lockfile doesn't want a dependency on
    // aube-registry just to load npmrc (and the historical behavior
    // never read `.npmrc` either).
    let Ok(raw) = aube_manifest::workspace::load_raw(project_dir) else {
        return false;
    };
    let npmrc: Vec<(String, String)> = Vec::new();
    let ctx = aube_settings::ResolveCtx::files_only(&npmrc, &raw);
    aube_settings::resolved::git_branch_lockfile(&ctx)
}

pub(crate) fn current_git_branch(project_dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(project_dir)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// Detect and parse the lockfile in the given project directory.
///
/// Priority: `aube-lock.yaml` → `pnpm-lock.yaml` → `bun.lock` →
/// `yarn.lock` → `npm-shrinkwrap.json` → `package-lock.json`.
/// (Shrinkwrap takes priority over package-lock.json when both exist, matching npm's behavior.)
///
/// `manifest` is needed to classify direct vs transitive deps when
/// reading yarn.lock (which has no notion of that distinction).
pub fn parse_lockfile(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let (graph, _kind) = parse_lockfile_with_kind(project_dir, manifest)?;
    Ok(graph)
}

/// Like [`parse_lockfile`] but also returns which format was read.
pub fn parse_lockfile_with_kind(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<(LockfileGraph, LockfileKind), Error> {
    reject_bun_binary(project_dir)?;
    for (path, kind) in lockfile_candidates(project_dir, /*include_aube=*/ true) {
        if !path.exists() {
            continue;
        }
        let kind = refine_yarn_kind(&path, kind);
        let graph = parse_one(&path, kind, manifest)?;
        return Ok((graph, kind));
    }
    Err(Error::NotFound(project_dir.to_path_buf()))
}

/// Variant of [`parse_lockfile_with_kind`] used by `aube import`.
///
/// Skips `aube-lock.yaml` — if the project already has one, there's
/// nothing to import. `pnpm-lock.yaml` *is* included because the whole
/// point of `aube import` is to convert a foreign lockfile (including
/// pnpm's) into `aube-lock.yaml`.
pub fn parse_for_import(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
) -> Result<(LockfileGraph, LockfileKind), Error> {
    reject_bun_binary(project_dir)?;
    for (path, kind) in lockfile_candidates(project_dir, /*include_aube=*/ false) {
        if !path.exists() {
            continue;
        }
        let kind = refine_yarn_kind(&path, kind);
        let graph = parse_one(&path, kind, manifest)?;
        return Ok((graph, kind));
    }
    Err(Error::NotFound(project_dir.to_path_buf()))
}

/// If only `bun.lockb` is present (without a text `bun.lock`), surface an
/// actionable error instead of silently falling through to another format.
fn reject_bun_binary(project_dir: &Path) -> Result<(), Error> {
    let lockb = project_dir.join("bun.lockb");
    let text = project_dir.join("bun.lock");
    if lockb.exists() && !text.exists() {
        return Err(Error::parse(
            &lockb,
            "bun.lockb (binary format) is not supported — run `bun install --save-text-lockfile` to generate a bun.lock text file first, or upgrade to bun 1.2+ where text is the default",
        ));
    }
    Ok(())
}

fn lockfile_candidates(project_dir: &Path, include_aube: bool) -> Vec<(PathBuf, LockfileKind)> {
    let mut out = Vec::new();
    if include_aube {
        // Prefer the branch-specific lockfile (if `gitBranchLockfile` is on
        // and we resolve a branch); fall through to plain `aube-lock.yaml`
        // so a freshly-enabled branch still picks up the base lockfile.
        let branch_name = aube_lock_filename(project_dir);
        if branch_name != "aube-lock.yaml" {
            out.push((project_dir.join(&branch_name), LockfileKind::Aube));
        }
        out.push((project_dir.join("aube-lock.yaml"), LockfileKind::Aube));
    }
    // Preserve pnpm lockfiles in place. Branch-specific
    // `pnpm-lock.<branch>.yaml` mirrors the aube branch lockfile naming
    // logic, so a project that already uses pnpm branch lockfiles keeps
    // writing through that file.
    let pnpm_branch = {
        let mut s = aube_lock_filename(project_dir);
        if let Some(rest) = s.strip_prefix("aube-lock.") {
            s = format!("pnpm-lock.{rest}");
        }
        s
    };
    if pnpm_branch != "pnpm-lock.yaml" {
        out.push((project_dir.join(&pnpm_branch), LockfileKind::Pnpm));
    }
    out.push((project_dir.join("pnpm-lock.yaml"), LockfileKind::Pnpm));
    out.push((project_dir.join("bun.lock"), LockfileKind::Bun));
    out.push((project_dir.join("yarn.lock"), LockfileKind::Yarn));
    out.push((
        project_dir.join("npm-shrinkwrap.json"),
        LockfileKind::NpmShrinkwrap,
    ));
    out.push((project_dir.join("package-lock.json"), LockfileKind::Npm));
    out
}

fn parse_one(
    path: &Path,
    kind: LockfileKind,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    match kind {
        // `aube-lock.yaml` uses the same on-disk format as pnpm v9 for
        // now — same parser, same writer — so we piggyback on the pnpm
        // module. Keeping the variant distinct lets detection/import
        // treat the two differently even though the bytes are the same.
        LockfileKind::Aube | LockfileKind::Pnpm => pnpm::parse(path),
        // yarn.rs::parse peeks the file for `__metadata:` and
        // dispatches between classic (v1) and berry (v2+) internally,
        // so we can hand both kinds to the same entry point. The
        // caller keeps the kind label it resolved from
        // `refine_yarn_kind` for downstream write-back.
        LockfileKind::Yarn | LockfileKind::YarnBerry => yarn::parse(path, manifest),
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => npm::parse(path),
        LockfileKind::Bun => bun::parse(path),
    }
}

/// Replace `LockfileKind::Yarn` with `LockfileKind::YarnBerry` when
/// the yarn.lock at `path` is actually a yarn 2+ lockfile. Other
/// kinds pass through unchanged.
///
/// `lockfile_candidates` only knows filenames, not content, so the
/// yarn entry is always tagged `Yarn`. Callers that need the precise
/// variant (install write-back, import conversions, drift logging)
/// funnel through this helper after confirming the candidate exists.
fn refine_yarn_kind(path: &Path, kind: LockfileKind) -> LockfileKind {
    if kind == LockfileKind::Yarn && yarn::is_berry_path(path) {
        LockfileKind::YarnBerry
    } else {
        kind
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("no lockfile found in {0}")]
    NotFound(std::path::PathBuf),
    #[error("unsupported lockfile format: {0}")]
    UnsupportedFormat(String),
    #[error("failed to read lockfile {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),
    /// Structural/serialization lockfile errors that have no source
    /// location — shape checks (`must be a mapping`), version guards
    /// (`lockfileVersion N unsupported`), and `serde_yaml::to_string`
    /// failures during write.
    #[error("failed to parse lockfile {0}: {1}")]
    Parse(std::path::PathBuf, String),
    /// Deserialization failure with a byte offset into the source
    /// content, so miette's `fancy` handler can draw a pointer at the
    /// offending byte of the lockfile. Reuses `aube_manifest`'s
    /// `ParseError` — identical shape, identical rendering — via the
    /// same `ParseDiag` pattern `aube-workspace` uses.
    #[error(transparent)]
    #[diagnostic(transparent)]
    ParseDiag(Box<aube_manifest::ParseError>),
}

/// Read a lockfile from disk, mapping I/O errors to `Error::Io`.
pub fn read_lockfile(path: &std::path::Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Parse a JSON lockfile document, attaching a miette source span on
/// failure so the fancy handler can point at the offending byte.
pub fn parse_json<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    content: String,
) -> Result<T, Error> {
    let mut buf = content.clone().into_bytes();
    match simd_json::serde::from_slice(&mut buf) {
        Ok(v) => Ok(v),
        Err(_) => match serde_json::from_str(&content) {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::parse_json_err(path, content, &e)),
        },
    }
}

impl Error {
    pub fn parse(path: &std::path::Path, msg: impl Into<String>) -> Self {
        Error::Parse(path.to_path_buf(), msg.into())
    }

    pub fn parse_json_err(
        path: &std::path::Path,
        content: String,
        err: &serde_json::Error,
    ) -> Self {
        Error::ParseDiag(Box::new(aube_manifest::ParseError::from_json_err(
            path, content, err,
        )))
    }

    pub fn parse_yaml_err(
        path: &std::path::Path,
        content: String,
        err: &serde_yaml::Error,
    ) -> Self {
        Error::ParseDiag(Box::new(aube_manifest::ParseError::from_yaml_err(
            path, content, err,
        )))
    }
}

#[cfg(test)]
mod has_bin_metadata_tests {
    use super::*;

    fn pkg_with_bin(bin: BTreeMap<String, String>) -> LockedPackage {
        LockedPackage {
            name: "p".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "p@1.0.0".to_string(),
            bin,
            ..Default::default()
        }
    }

    #[test]
    fn empty_graph_has_no_bin_metadata() {
        let g = LockfileGraph::default();
        assert!(!g.has_bin_metadata());
    }

    #[test]
    fn graph_with_no_bins_returns_false() {
        let mut g = LockfileGraph::default();
        g.packages
            .insert("a@1.0.0".to_string(), pkg_with_bin(BTreeMap::new()));
        g.packages
            .insert("b@2.0.0".to_string(), pkg_with_bin(BTreeMap::new()));
        assert!(!g.has_bin_metadata());
    }

    #[test]
    fn any_non_empty_bin_flips_to_true() {
        let mut g = LockfileGraph::default();
        g.packages
            .insert("a@1.0.0".to_string(), pkg_with_bin(BTreeMap::new()));
        let mut bin = BTreeMap::new();
        bin.insert("tool".to_string(), "bin/tool.js".to_string());
        g.packages.insert("b@2.0.0".to_string(), pkg_with_bin(bin));
        assert!(g.has_bin_metadata());
    }

    /// pnpm parsers record `hasBin: true` as a one-entry placeholder
    /// map (empty key + empty value). That still flips the flag.
    #[test]
    fn pnpm_placeholder_bin_counts() {
        let mut g = LockfileGraph::default();
        let mut bin = BTreeMap::new();
        bin.insert(String::new(), String::new());
        g.packages.insert("a@1.0.0".to_string(), pkg_with_bin(bin));
        assert!(g.has_bin_metadata());
    }
}

#[cfg(test)]
mod parse_diag_tests {
    use super::*;
    use std::path::Path;

    /// Trailing `,` in an otherwise fine JSON lockfile — confirm the
    /// helper attaches a `NamedSource` pointed at the lockfile path and
    /// the span stays in bounds so miette can render a pointer.
    #[test]
    fn parse_json_attaches_span_for_bad_input() {
        let path = Path::new("package-lock.json");
        let content = r#"{"name":"x","#.to_string();
        let Err(Error::ParseDiag(pe)) = parse_json::<serde_json::Value>(path, content.clone())
        else {
            panic!("parse_json must produce ParseDiag on malformed input");
        };
        let offset: usize = pe.span.offset();
        let len: usize = pe.span.len();
        assert!(offset + len <= content.len());
        assert_eq!(pe.path, path);
    }

    /// Same story for YAML — serde_yaml reports a `Location` with a
    /// byte index directly, so no line/col conversion is exercised
    /// here. Both production sites (`pnpm.rs`, `yarn.rs`) call
    /// `Error::parse_yaml_err` directly (one iterates multiple YAML
    /// documents, the other has only borrowed content), so that's the
    /// entry point this test locks down.
    #[test]
    fn parse_yaml_err_attaches_span_for_bad_input() {
        let path = Path::new("yarn.lock");
        let content = "packages:\n\t- pkg\n".to_string();
        let yaml_err: serde_yaml::Error = serde_yaml::from_str::<serde_yaml::Value>(&content)
            .expect_err("tab-indented YAML must fail");
        let Error::ParseDiag(pe) = Error::parse_yaml_err(path, content.clone(), &yaml_err) else {
            panic!("parse_yaml_err must produce ParseDiag");
        };
        let offset: usize = pe.span.offset();
        let len: usize = pe.span.len();
        assert!(offset + len <= content.len());
        assert_eq!(pe.path, path);
    }
}

#[cfg(test)]
mod looks_like_remote_tarball_url_tests {
    use super::*;

    #[test]
    fn matches_https_tgz() {
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://example.com/pkg-1.0.0.tgz"
        ));
    }

    #[test]
    fn matches_http_tar_gz() {
        assert!(LocalSource::looks_like_remote_tarball_url(
            "http://example.com/pkg-1.0.0.tar.gz"
        ));
    }

    #[test]
    fn strips_fragment_before_suffix_check() {
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://example.com/pkg-1.0.0.tgz#sha512-abc"
        ));
    }

    #[test]
    fn strips_query_string_before_suffix_check() {
        // Auth-token URLs from private registries (JFrog, Nexus,
        // CodeArtifact, …) routinely trail `?token=…` after the
        // filename. Must still classify as a tarball URL.
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://registry.example.com/pkg/-/pkg-1.0.0.tgz?token=abc"
        ));
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://example.com/pkg-1.0.0.tar.gz?v=2&signed=1"
        ));
    }

    #[test]
    fn matches_bare_http_url_without_tarball_suffix() {
        // pkg.pr.new serves tarballs from URLs without a `.tgz`
        // extension; npm treats all non-git http(s) URLs as tarball
        // URLs, so these must classify as remote tarballs.
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://pkg.pr.new/lunariajs/lunaria/@lunariajs/core@904b935"
        ));
        assert!(LocalSource::looks_like_remote_tarball_url(
            "https://codeload.github.com/user/repo/tar.gz/main"
        ));
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(!LocalSource::looks_like_remote_tarball_url(
            "ftp://example.com/pkg.tgz"
        ));
        assert!(!LocalSource::looks_like_remote_tarball_url(
            "git://example.com/repo.git"
        ));
    }

    #[test]
    fn parse_classifies_bare_http_url_as_remote_tarball() {
        use std::path::Path;
        let parsed = LocalSource::parse(
            "https://pkg.pr.new/lunariajs/lunaria/@lunariajs/core@904b935",
            Path::new(""),
        );
        assert!(matches!(parsed, Some(LocalSource::RemoteTarball(_))));
    }

    #[test]
    fn parse_prefers_git_over_tarball_for_dot_git_url() {
        use std::path::Path;
        let parsed = LocalSource::parse("https://github.com/user/repo.git", Path::new(""));
        assert!(matches!(parsed, Some(LocalSource::Git(_))));
    }
}

#[cfg(test)]
mod filename_tests {
    use super::*;

    #[test]
    fn defaults_to_plain_lockfile_when_setting_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(aube_lock_filename(dir.path()), "aube-lock.yaml");
        assert_eq!(pnpm_lock_filename(dir.path()), "pnpm-lock.yaml");
    }

    #[test]
    fn defaults_to_plain_lockfile_when_setting_explicit_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "gitBranchLockfile: false\n",
        )
        .unwrap();
        assert_eq!(aube_lock_filename(dir.path()), "aube-lock.yaml");
    }

    #[test]
    fn uses_branch_filename_when_enabled_inside_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "gitBranchLockfile: true\n",
        )
        .unwrap();
        // git init + checkout a branch with a `/` so we exercise the
        // pnpm-style `!` encoding.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C"])
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
        };
        if run(&["init", "-q"]).status.success() {
            run(&["checkout", "-q", "-b", "feature/x"]);
            assert_eq!(aube_lock_filename(dir.path()), "aube-lock.feature!x.yaml");
            assert_eq!(pnpm_lock_filename(dir.path()), "pnpm-lock.feature!x.yaml");
        }
    }
}

#[cfg(test)]
mod git_spec_tests {
    use super::*;

    #[test]
    fn git_plus_https_without_dot_git_roundtrips_via_lockfile_form() {
        // Initial parse: `git+https://…/repo` (no `.git`).
        let (url, committish, subpath) = parse_git_spec("git+https://host/user/repo").unwrap();
        assert_eq!(url, "https://host/user/repo");
        assert_eq!(committish, None);
        assert_eq!(subpath, None);

        // After resolving, the serializer writes `<url>#<sha>` into
        // the lockfile's importer `version:` field.
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let source = LocalSource::Git(GitSource {
            url: url.clone(),
            committish: None,
            resolved: sha.to_string(),
            subpath: None,
        });
        let lockfile_version = source.specifier();
        assert_eq!(lockfile_version, format!("https://host/user/repo#{sha}"));

        // Re-parse must recognize the bare URL because the 40-hex
        // committish suffix unambiguously tags it as git.
        let (round_url, round_committish, round_subpath) =
            parse_git_spec(&lockfile_version).unwrap();
        assert_eq!(round_url, "https://host/user/repo");
        assert_eq!(round_committish.as_deref(), Some(sha));
        assert_eq!(round_subpath, None);
    }

    #[test]
    fn bare_https_without_dot_git_and_no_committish_is_not_git() {
        // A plain `https://…` URL with no `.git` and no SHA could be
        // anything (including a tarball); don't claim it.
        assert!(parse_git_spec("https://example.com/pkg").is_none());
    }

    #[test]
    fn github_shorthand_expands_and_roundtrips() {
        let (url, _, _) = parse_git_spec("github:user/repo").unwrap();
        assert_eq!(url, "https://github.com/user/repo.git");
    }

    #[test]
    fn commit_selector_fragment_normalizes_to_sha() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let (url, committish, _) =
            parse_git_spec(&format!("https://host/user/repo.git#commit={sha}")).unwrap();
        assert_eq!(url, "https://host/user/repo.git");
        assert_eq!(committish.as_deref(), Some(sha));
    }

    #[test]
    fn named_selector_fragment_normalizes_to_ref() {
        let (url, committish, _) = parse_git_spec("git+https://host/user/repo#tag=v1.2.3").unwrap();
        assert_eq!(url, "https://host/user/repo");
        assert_eq!(committish.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn pnpm_path_subpath_extracted_from_fragment() {
        // pnpm syntax: `<url>#<ref>&path:/<subdir>` selects a
        // subdirectory of the cloned repo as the package root.
        let (url, committish, subpath) =
            parse_git_spec("github:org/dep#v0.1.4&path:/packages/special").unwrap();
        assert_eq!(url, "https://github.com/org/dep.git");
        assert_eq!(committish.as_deref(), Some("v0.1.4"));
        assert_eq!(subpath.as_deref(), Some("packages/special"));
    }

    #[test]
    fn path_subpath_roundtrips_via_specifier() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let source = LocalSource::Git(GitSource {
            url: "https://github.com/org/dep.git".to_string(),
            committish: None,
            resolved: sha.to_string(),
            subpath: Some("packages/special".to_string()),
        });
        let spec = source.specifier();
        assert_eq!(
            spec,
            format!("https://github.com/org/dep.git#{sha}&path:/packages/special")
        );
        let (url, committish, subpath) = parse_git_spec(&spec).unwrap();
        assert_eq!(url, "https://github.com/org/dep.git");
        assert_eq!(committish.as_deref(), Some(sha));
        assert_eq!(subpath.as_deref(), Some("packages/special"));
    }

    #[test]
    fn dep_path_distinguishes_subpaths_under_same_commit() {
        // Two packages from the same repo+commit but different
        // subdirs must hash to distinct dep_paths so the linker
        // doesn't collapse them.
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let a = LocalSource::Git(GitSource {
            url: "https://example.com/r.git".to_string(),
            committish: None,
            resolved: sha.to_string(),
            subpath: Some("packages/a".to_string()),
        });
        let b = LocalSource::Git(GitSource {
            url: "https://example.com/r.git".to_string(),
            committish: None,
            resolved: sha.to_string(),
            subpath: Some("packages/b".to_string()),
        });
        assert_ne!(a.dep_path("dep"), b.dep_path("dep"));
    }
}

#[cfg(test)]
mod drift_tests {
    use super::*;
    use aube_manifest::PackageJson;
    use std::collections::BTreeMap;

    fn make_manifest(deps: &[(&str, &str)]) -> PackageJson {
        let mut m = PackageJson {
            name: Some("test".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };
        for (name, spec) in deps {
            m.dependencies.insert((*name).into(), (*spec).into());
        }
        m
    }

    fn make_graph(deps: &[(&str, &str, &str)]) -> LockfileGraph {
        // (name, specifier, dep_path)
        let direct: Vec<DirectDep> = deps
            .iter()
            .map(|(name, spec, dep_path)| DirectDep {
                name: (*name).into(),
                dep_path: (*dep_path).into(),
                dep_type: DepType::Production,
                specifier: Some((*spec).into()),
            })
            .collect();
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), direct);
        LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn fresh_when_specifiers_match() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_specifier_changes() {
        let manifest = make_manifest(&[("lodash", "^4.18.0")]);
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("lodash")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_when_manifest_adds_dep() {
        let manifest = make_manifest(&[("lodash", "^4.17.0"), ("express", "^4.18.0")]);
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("express")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_when_manifest_removes_dep() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = make_graph(&[
            ("lodash", "^4.17.0", "lodash@4.17.21"),
            ("express", "^4.18.0", "express@4.18.0"),
        ]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("express")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    // Regression guard for #42: the drift check must recognize
    // auto-hoisted peers as derived state, not as "manifest removed X".
    // Without this, every project that has any peer dep would trigger
    // a full re-resolve on every install, defeating lockfile caching.
    #[test]
    fn fresh_when_lockfile_has_auto_hoisted_peer() {
        let manifest = make_manifest(&[("use-sync-external-store", "1.2.0")]);
        let mut graph = make_graph(&[
            (
                "use-sync-external-store",
                "1.2.0",
                "use-sync-external-store@1.2.0",
            ),
            // Hoisted peer — in the lockfile importers but not in the
            // user's package.json.
            ("react", "^16.8.0 || ^17.0.0 || ^18.0.0", "react@18.3.1"),
        ]);
        // The declaring package must list react as a peer for the
        // drift check to recognize the hoist. We add that here.
        let mut declaring_pkg = LockedPackage {
            name: "use-sync-external-store".into(),
            version: "1.2.0".into(),
            dep_path: "use-sync-external-store@1.2.0".into(),
            ..Default::default()
        };
        declaring_pkg
            .peer_dependencies
            .insert("react".into(), "^16.8.0 || ^17.0.0 || ^18.0.0".into());
        graph
            .packages
            .insert("use-sync-external-store@1.2.0".into(), declaring_pkg);

        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    // Regression: when a user explicitly pinned a dep that also happens
    // to share its name with a peer declaration elsewhere in the graph,
    // removing that pin from package.json must still be flagged as
    // stale — otherwise the old pinned version gets locked forever.
    // The check must key on (name, specifier), not name alone.
    #[test]
    fn stale_when_user_removes_pinned_dep_that_shares_name_with_a_peer() {
        // Manifest after the user removed react entirely. Only
        // use-sync-external-store remains.
        let manifest = make_manifest(&[("use-sync-external-store", "1.2.0")]);

        // Lockfile still has the user's old `react: 17.0.2` pin alongside
        // use-sync-external-store. Pre-removal state.
        let mut graph = make_graph(&[
            (
                "use-sync-external-store",
                "1.2.0",
                "use-sync-external-store@1.2.0",
            ),
            ("react", "17.0.2", "react@17.0.2"),
        ]);
        // Add the peer declaration on the consumer package. This is
        // the case that previously defeated the name-only check:
        // react's specifier "17.0.2" doesn't match the declared peer
        // range, so the hoist recognizer must reject it.
        let mut consumer = LockedPackage {
            name: "use-sync-external-store".into(),
            version: "1.2.0".into(),
            dep_path: "use-sync-external-store@1.2.0".into(),
            ..Default::default()
        };
        consumer
            .peer_dependencies
            .insert("react".into(), "^16.8.0 || ^17.0.0 || ^18.0.0".into());
        graph
            .packages
            .insert("use-sync-external-store@1.2.0".into(), consumer);

        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("react")),
            DriftStatus::Fresh => panic!(
                "drift check should flag a removed user-pinned dep as stale, \
                 even when its name matches a peer declaration"
            ),
        }
    }

    // But if the lockfile has a user-removed dep that ISN'T declared as a
    // peer anywhere, we still need to flag it as stale.
    #[test]
    fn stale_when_lockfile_has_removed_non_peer_dep() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = make_graph(&[
            ("lodash", "^4.17.0", "lodash@4.17.21"),
            ("chalk", "^5.0.0", "chalk@5.0.0"),
        ]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("chalk")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn fresh_when_no_specifiers_recorded() {
        // Non-pnpm formats (npm/yarn/bun) don't store specifiers, so we can't
        // detect drift — we treat them as fresh and let the resolver decide.
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let graph = LockfileGraph {
            importers: {
                let mut m = BTreeMap::new();
                m.insert(
                    ".".to_string(),
                    vec![DirectDep {
                        name: "lodash".into(),
                        dep_path: "lodash@4.17.21".into(),
                        dep_type: DepType::Production,
                        specifier: None,
                    }],
                );
                m
            },
            packages: BTreeMap::new(),
            ..Default::default()
        };
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_manifest_adds_override() {
        // Lockfile recorded no overrides; manifest now has one. Drift
        // must fire so the next install re-runs the resolver and bakes
        // the override into the graph.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"lodash": "4.17.21"}));
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("overrides")),
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_drift_message_names_changed_override_key() {
        // Both sides have one entry, but the value differs. The reason
        // should name the key — the previous "lockfile: 1 entries,
        // manifest: 1 entries" message looked like nothing changed.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"lodash": "5.0.0"}));
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("lodash"), "expected key in: {reason}");
                assert!(
                    reason.contains("4.17.21"),
                    "expected old value in: {reason}"
                );
                assert!(reason.contains("5.0.0"), "expected new value in: {reason}");
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn stale_when_manifest_removes_override() {
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("removes"));
                assert!(reason.contains("lodash"));
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }
    }

    #[test]
    fn fresh_when_overrides_match() {
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"lodash": "4.17.21"}));
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn fresh_when_workspace_yaml_overrides_match_lockfile() {
        // pnpm v10 moved `overrides` to pnpm-workspace.yaml. When the
        // resolver wrote them into `self.overrides`, the drift check
        // must see the same map — otherwise the second install run
        // rejects the lockfile as stale with "manifest removes ..."
        // (reported in discussion #174).
        let manifest = make_manifest(&[("semver", "^7.5.0")]);
        let mut graph = make_graph(&[("semver", "^7.5.0", "semver@7.7.1")]);
        graph.overrides.insert("semver".into(), "7.7.1".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("semver".into(), "7.7.1".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn workspace_yaml_overrides_win_over_package_json() {
        // When both pnpm-workspace.yaml and package.json declare an
        // override for the same key, the workspace yaml wins — pnpm
        // v10's precedence. The drift check must apply the merged
        // effective map.
        let mut manifest = make_manifest(&[("semver", "^7.5.0")]);
        manifest
            .extra
            .insert("overrides".into(), serde_json::json!({"semver": "7.0.0"}));
        let mut graph = make_graph(&[("semver", "^7.5.0", "semver@7.7.1")]);
        graph.overrides.insert("semver".into(), "7.7.1".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("semver".into(), "7.7.1".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_override_catalog_ref_matches_lockfile_resolved() {
        // pnpm-workspace.yaml: `overrides: { lodash: "catalog:" }` with
        // `catalog: { lodash: 4.17.21 }`. pnpm writes the lockfile with
        // the resolved override value (`lodash: 4.17.21`), so a frozen
        // install comparing the raw `catalog:` string against the
        // resolved form would always read stale (discussion #174).
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "catalog:".into());
        let mut catalogs = BTreeMap::new();
        let mut default_cat = BTreeMap::new();
        default_cat.insert("lodash".into(), "4.17.21".into());
        catalogs.insert("default".into(), default_cat);
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &catalogs),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_override_named_catalog_ref_matches_lockfile_resolved() {
        // Named catalog variant: `overrides: { lodash: "catalog:evens" }`
        // resolves against `catalogs.evens.lodash`.
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "catalog:evens".into());
        let mut catalogs = BTreeMap::new();
        let mut evens = BTreeMap::new();
        evens.insert("lodash".into(), "4.17.21".into());
        catalogs.insert("evens".into(), evens);
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &catalogs),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn stale_when_override_catalog_ref_diverges_from_lockfile() {
        // If the catalog moves to a new version, the resolved override
        // no longer matches the lockfile — drift must fire, not silently
        // accept.
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "catalog:".into());
        let mut catalogs = BTreeMap::new();
        let mut default_cat = BTreeMap::new();
        default_cat.insert("lodash".into(), "4.17.22".into());
        catalogs.insert("default".into(), default_cat);
        match graph.check_drift(&manifest, &ws_overrides, &[], &catalogs) {
            DriftStatus::Stale { reason } => assert!(reason.contains("lodash")),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn fresh_when_pnpm_wrote_override_rewritten_importer_spec() {
        // pnpm rewrites the importer `specifier:` to the post-override
        // value when a bare-name override applies, so a pnpm-generated
        // lockfile records `specifier: 4.17.21` even though
        // `package.json` still reads `^4.17.0`. Without override-aware
        // drift, every frozen install against a pnpm lockfile with
        // overrides reads stale (discussion #174).
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "lodash".into(),
                dep_path: "lodash@4.17.21".into(),
                dep_type: DepType::Production,
                specifier: Some("4.17.21".into()),
            }],
        );
        let mut graph = LockfileGraph {
            importers,
            ..Default::default()
        };
        graph.overrides.insert("lodash".into(), "4.17.21".into());
        let mut ws_overrides = BTreeMap::new();
        ws_overrides.insert("lodash".into(), "4.17.21".into());
        assert_eq!(
            graph.check_drift(&manifest, &ws_overrides, &[], &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_workspace_yaml_ignored_optional_matches_lockfile() {
        // Same drift-shaped bug as overrides: the resolver unions
        // `ignoredOptionalDependencies` from package.json and
        // pnpm-workspace.yaml, so the lockfile's
        // `ignored_optional_dependencies` carries the union, and the
        // drift check has to see the same union or the next
        // `--frozen-lockfile` run fails with "manifest removes".
        let manifest = make_manifest(&[("lodash", "^4.17.0")]);
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        graph
            .ignored_optional_dependencies
            .insert("fsevents".to_string());
        let ws_ignored = vec!["fsevents".to_string()];
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &ws_ignored, &BTreeMap::new()),
            DriftStatus::Fresh,
        );
    }

    #[test]
    fn fresh_when_optional_dep_was_recorded_as_skipped() {
        // Regression: a platform-skipped optional dep would otherwise
        // loop forever as "manifest adds X". When the previous
        // resolve recorded it under skipped_optional_dependencies with
        // a matching specifier, drift must report Fresh.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.3.0".into());
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        let mut inner = BTreeMap::new();
        inner.insert("fsevents".to_string(), "^2.3.0".to_string());
        graph
            .skipped_optional_dependencies
            .insert(".".to_string(), inner);
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn stale_when_new_optional_dep_was_never_seen() {
        // Cursor Bugbot regression: a brand-new optional dep that the
        // previous resolve never saw must trigger drift, otherwise it
        // would silently never get installed. Distinct from a
        // platform-skipped optional, which has an entry in
        // `skipped_optional_dependencies`.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.3.0".into());
        let graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => panic!("expected Stale on new optional dep"),
        }
    }

    #[test]
    fn stale_when_skipped_optional_dep_specifier_changes() {
        // The user bumped the range on a previously-skipped optional;
        // the recorded specifier no longer matches the manifest, so we
        // need to re-resolve.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0")]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.4.0".into());
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        let mut inner = BTreeMap::new();
        inner.insert("fsevents".to_string(), "^2.3.0".to_string());
        graph
            .skipped_optional_dependencies
            .insert(".".to_string(), inner);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => panic!("expected Stale on skipped optional spec change"),
        }
    }

    #[test]
    fn stale_when_skipped_optional_is_promoted_to_required() {
        // Cursor Bugbot regression: if the user moves a previously-
        // skipped optional into `dependencies` (same specifier), the
        // skipped-list exemption must NOT fire — the dep is now
        // required and the lockfile genuinely doesn't include it.
        let mut manifest = make_manifest(&[("lodash", "^4.17.0"), ("fsevents", "^2.3.0")]);
        // Note: fsevents lives in `dependencies`, not
        // `optional_dependencies`, even though the lockfile recorded
        // it under skipped optionals from a previous resolve.
        manifest.optional_dependencies.clear();
        let mut graph = make_graph(&[("lodash", "^4.17.0", "lodash@4.17.21")]);
        let mut inner = BTreeMap::new();
        inner.insert("fsevents".to_string(), "^2.3.0".to_string());
        graph
            .skipped_optional_dependencies
            .insert(".".to_string(), inner);
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => {
                panic!("expected Stale: skipped-optional exemption must not apply to required deps")
            }
        }
    }

    #[test]
    fn stale_when_optional_dep_specifier_changes_in_lockfile() {
        // Spec changes on optionals that *are* present must still
        // drift, so the resolver re-runs when the user bumps a range.
        let mut manifest = make_manifest(&[]);
        manifest
            .optional_dependencies
            .insert("fsevents".into(), "^2.4.0".into());
        let mut graph = make_graph(&[]);
        graph.importers.get_mut(".").unwrap().push(DirectDep {
            name: "fsevents".into(),
            dep_path: "fsevents@2.3.0".into(),
            dep_type: DepType::Optional,
            specifier: Some("^2.3.0".into()),
        });
        match graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()) {
            DriftStatus::Stale { reason } => assert!(reason.contains("fsevents"), "{reason}"),
            DriftStatus::Fresh => panic!("expected Stale on optional spec change"),
        }
    }

    #[test]
    fn fresh_for_empty_manifest_and_lockfile() {
        let manifest = make_manifest(&[]);
        let graph = make_graph(&[]);
        assert_eq!(
            graph.check_drift(&manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn workspace_drift_detects_change_in_non_root_importer() {
        // Build a graph with two importers: root and packages/app.
        let root_dep = DirectDep {
            name: "lodash".into(),
            dep_path: "lodash@4.17.21".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.17.0".into()),
        };
        let app_dep = DirectDep {
            name: "express".into(),
            dep_path: "express@4.18.0".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.18.0".into()),
        };
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![root_dep]);
        importers.insert("packages/app".to_string(), vec![app_dep]);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        };

        let root_manifest = make_manifest(&[("lodash", "^4.17.0")]);
        // App manifest changed express to ^5.0.0 — should be detected as stale.
        let app_manifest = make_manifest(&[("express", "^5.0.0")]);

        let workspace_manifests = vec![
            (".".to_string(), root_manifest.clone()),
            ("packages/app".to_string(), app_manifest),
        ];
        match graph.check_drift_workspace(
            &workspace_manifests,
            &BTreeMap::new(),
            &[],
            &BTreeMap::new(),
        ) {
            DriftStatus::Stale { reason } => {
                assert!(reason.contains("packages/app"));
                assert!(reason.contains("express"));
            }
            DriftStatus::Fresh => panic!("expected Stale"),
        }

        // Single-importer check_drift on root only would say Fresh.
        assert_eq!(
            graph.check_drift(&root_manifest, &BTreeMap::new(), &[], &BTreeMap::new()),
            DriftStatus::Fresh
        );
    }

    #[test]
    fn filter_deps_prunes_dev_only_subtree() {
        // Graph: prod-root (foo) + dev-root (jest) with transitive chains.
        // After filtering out Dev, jest + its transitives should be pruned,
        // foo + its transitives should remain.
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".into(),
                    dep_path: "foo@1.0.0".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".into()),
                },
                DirectDep {
                    name: "jest".into(),
                    dep_path: "jest@29.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^29.0.0".into()),
                },
            ],
        );

        let mut packages = BTreeMap::new();
        let mut foo_deps = BTreeMap::new();
        foo_deps.insert("bar".to_string(), "2.0.0".to_string());
        packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".into(),
                version: "1.0.0".into(),
                integrity: None,
                dependencies: foo_deps,
                dep_path: "foo@1.0.0".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "bar@2.0.0".to_string(),
            LockedPackage {
                name: "bar".into(),
                version: "2.0.0".into(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: "bar@2.0.0".into(),
                ..Default::default()
            },
        );
        let mut jest_deps = BTreeMap::new();
        jest_deps.insert("jest-core".to_string(), "29.0.0".to_string());
        packages.insert(
            "jest@29.0.0".to_string(),
            LockedPackage {
                name: "jest".into(),
                version: "29.0.0".into(),
                integrity: None,
                dependencies: jest_deps,
                dep_path: "jest@29.0.0".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "jest-core@29.0.0".to_string(),
            LockedPackage {
                name: "jest-core".into(),
                version: "29.0.0".into(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: "jest-core@29.0.0".into(),
                ..Default::default()
            },
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let prod = graph.filter_deps(|d| d.dep_type != DepType::Dev);

        // Direct deps: only foo, jest dropped
        let roots = prod.root_deps();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "foo");

        // Reachable packages: foo + bar (transitive), NOT jest or jest-core
        assert!(prod.packages.contains_key("foo@1.0.0"));
        assert!(prod.packages.contains_key("bar@2.0.0"));
        assert!(!prod.packages.contains_key("jest@29.0.0"));
        assert!(!prod.packages.contains_key("jest-core@29.0.0"));
    }

    // Regression for #50 feedback: `filter_deps` is a structural
    // operation and must preserve the source graph's `settings:`
    // metadata. A filtered graph that's handed to the lockfile writer
    // (as `aube prune` does today) would otherwise reset
    // `autoInstallPeers` to its default and silently flip the user's
    // choice on the next install.
    #[test]
    fn filter_deps_preserves_lockfile_settings() {
        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages: BTreeMap::new(),
            settings: LockfileSettings {
                auto_install_peers: false,
                exclude_links_from_lockfile: true,
                lockfile_include_tarball_url: false,
            },
            ..Default::default()
        };
        let filtered = graph.filter_deps(|_| true);
        assert!(!filtered.settings.auto_install_peers);
        assert!(filtered.settings.exclude_links_from_lockfile);
    }

    #[test]
    fn filter_deps_keeps_shared_transitive_reachable_via_prod() {
        // Graph: prod foo → shared, dev jest → shared
        // Filtering out Dev should still keep `shared` because foo → shared
        // keeps it reachable.
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".into(),
                    dep_path: "foo@1.0.0".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".into()),
                },
                DirectDep {
                    name: "jest".into(),
                    dep_path: "jest@29.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^29.0.0".into()),
                },
            ],
        );

        let mut packages = BTreeMap::new();
        for (name, ver, deps) in [
            ("foo", "1.0.0", vec![("shared", "1.0.0")]),
            ("jest", "29.0.0", vec![("shared", "1.0.0")]),
            ("shared", "1.0.0", vec![]),
        ] {
            let mut dep_map = BTreeMap::new();
            for (n, v) in deps {
                dep_map.insert(n.to_string(), v.to_string());
            }
            packages.insert(
                format!("{name}@{ver}"),
                LockedPackage {
                    name: name.into(),
                    version: ver.into(),
                    integrity: None,
                    dependencies: dep_map,
                    dep_path: format!("{name}@{ver}"),
                    ..Default::default()
                },
            );
        }

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let prod = graph.filter_deps(|d| d.dep_type != DepType::Dev);

        assert!(prod.packages.contains_key("foo@1.0.0"));
        assert!(prod.packages.contains_key("shared@1.0.0"));
        assert!(!prod.packages.contains_key("jest@29.0.0"));
    }

    #[test]
    fn subset_to_importer_returns_none_for_missing_importer() {
        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages: BTreeMap::new(),
            ..Default::default()
        };
        assert!(graph.subset_to_importer("packages/lib", |_| true).is_none());
    }

    #[test]
    fn subset_to_importer_keeps_only_requested_importer_transitive_closure() {
        // Workspace graph with two importers that own independent
        // subtrees: packages/lib pulls is-odd → is-number, packages/app
        // pulls express. Subsetting to packages/lib must yield a graph
        // rooted at `.` containing only is-odd + is-number, with
        // express pruned. Matches what `aube deploy --filter @test/lib`
        // should write into the target.
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![]);
        importers.insert(
            "packages/lib".to_string(),
            vec![DirectDep {
                name: "is-odd".into(),
                dep_path: "is-odd@3.0.1".into(),
                dep_type: DepType::Production,
                specifier: Some("^3.0.1".into()),
            }],
        );
        importers.insert(
            "packages/app".to_string(),
            vec![DirectDep {
                name: "express".into(),
                dep_path: "express@4.18.0".into(),
                dep_type: DepType::Production,
                specifier: Some("^4.18.0".into()),
            }],
        );

        let mut packages = BTreeMap::new();
        let mut is_odd_deps = BTreeMap::new();
        is_odd_deps.insert("is-number".to_string(), "6.0.0".to_string());
        packages.insert(
            "is-odd@3.0.1".to_string(),
            LockedPackage {
                name: "is-odd".into(),
                version: "3.0.1".into(),
                dependencies: is_odd_deps,
                dep_path: "is-odd@3.0.1".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "is-number@6.0.0".to_string(),
            LockedPackage {
                name: "is-number".into(),
                version: "6.0.0".into(),
                dep_path: "is-number@6.0.0".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "express@4.18.0".to_string(),
            LockedPackage {
                name: "express".into(),
                version: "4.18.0".into(),
                dep_path: "express@4.18.0".into(),
                ..Default::default()
            },
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let subset = graph
            .subset_to_importer("packages/lib", |_| true)
            .expect("packages/lib importer present");

        assert_eq!(subset.importers.len(), 1);
        let roots = subset.root_deps();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "is-odd");

        assert!(subset.packages.contains_key("is-odd@3.0.1"));
        assert!(subset.packages.contains_key("is-number@6.0.0"));
        assert!(!subset.packages.contains_key("express@4.18.0"));
    }

    #[test]
    fn subset_to_importer_honors_keep_predicate_for_prod_deploys() {
        // packages/lib has both prod (is-odd) and dev (jest) deps.
        // `aube deploy --prod` should pass `|d| d.dep_type != Dev` as
        // the keep filter; the resulting subset retains only is-odd
        // so drift against the target's dev-stripped manifest stays
        // clean.
        let mut importers = BTreeMap::new();
        importers.insert(
            "packages/lib".to_string(),
            vec![
                DirectDep {
                    name: "is-odd".into(),
                    dep_path: "is-odd@3.0.1".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^3.0.1".into()),
                },
                DirectDep {
                    name: "jest".into(),
                    dep_path: "jest@29.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^29.0.0".into()),
                },
            ],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            "is-odd@3.0.1".to_string(),
            LockedPackage {
                name: "is-odd".into(),
                version: "3.0.1".into(),
                dep_path: "is-odd@3.0.1".into(),
                ..Default::default()
            },
        );
        packages.insert(
            "jest@29.0.0".to_string(),
            LockedPackage {
                name: "jest".into(),
                version: "29.0.0".into(),
                dep_path: "jest@29.0.0".into(),
                ..Default::default()
            },
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let prod = graph
            .subset_to_importer("packages/lib", |d| d.dep_type != DepType::Dev)
            .expect("importer present");
        let roots = prod.root_deps();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "is-odd");
        assert!(prod.packages.contains_key("is-odd@3.0.1"));
        assert!(!prod.packages.contains_key("jest@29.0.0"));
    }

    #[test]
    fn subset_to_importer_preserves_graph_settings() {
        // Structural pruning, not a resolution-mode reset: a deploy
        // into a target that uses the source workspace's settings
        // header (autoInstallPeers / lockfileIncludeTarballUrl)
        // should write them through unchanged so a frozen install in
        // the target sees the same resolution-mode state.
        let mut importers = BTreeMap::new();
        importers.insert("packages/lib".to_string(), vec![]);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            settings: LockfileSettings {
                auto_install_peers: false,
                exclude_links_from_lockfile: true,
                lockfile_include_tarball_url: true,
            },
            ..Default::default()
        };
        let subset = graph.subset_to_importer("packages/lib", |_| true).unwrap();
        assert!(!subset.settings.auto_install_peers);
        assert!(subset.settings.exclude_links_from_lockfile);
        assert!(subset.settings.lockfile_include_tarball_url);
    }

    #[test]
    fn subset_to_importer_rekeys_skipped_optionals_to_root() {
        // `skipped_optional_dependencies` is per-importer. After
        // subsetting, only the retained importer's entry should
        // survive — rekeyed to `.` so a frozen install in the target
        // (which has exactly one importer) doesn't see ghost entries.
        let mut importers = BTreeMap::new();
        importers.insert("packages/lib".to_string(), vec![]);
        importers.insert("packages/app".to_string(), vec![]);
        let mut skipped = BTreeMap::new();
        let mut lib_skip = BTreeMap::new();
        lib_skip.insert("fsevents".to_string(), "^2".to_string());
        skipped.insert("packages/lib".to_string(), lib_skip);
        let mut app_skip = BTreeMap::new();
        app_skip.insert("ghost".to_string(), "*".to_string());
        skipped.insert("packages/app".to_string(), app_skip);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            skipped_optional_dependencies: skipped,
            ..Default::default()
        };
        let subset = graph.subset_to_importer("packages/lib", |_| true).unwrap();
        assert_eq!(subset.skipped_optional_dependencies.len(), 1);
        let root = subset.skipped_optional_dependencies.get(".").unwrap();
        assert!(root.contains_key("fsevents"));
        assert!(!root.contains_key("ghost"));
    }

    #[test]
    fn workspace_drift_fresh_when_all_importers_match() {
        let root_dep = DirectDep {
            name: "lodash".into(),
            dep_path: "lodash@4.17.21".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.17.0".into()),
        };
        let app_dep = DirectDep {
            name: "express".into(),
            dep_path: "express@4.18.0".into(),
            dep_type: DepType::Production,
            specifier: Some("^4.18.0".into()),
        };
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![root_dep]);
        importers.insert("packages/app".to_string(), vec![app_dep]);
        let graph = LockfileGraph {
            importers,
            packages: BTreeMap::new(),
            ..Default::default()
        };

        let workspace_manifests = vec![
            (".".to_string(), make_manifest(&[("lodash", "^4.17.0")])),
            (
                "packages/app".to_string(),
                make_manifest(&[("express", "^4.18.0")]),
            ),
        ];
        assert_eq!(
            graph.check_drift_workspace(
                &workspace_manifests,
                &BTreeMap::new(),
                &[],
                &BTreeMap::new()
            ),
            DriftStatus::Fresh
        );
    }

    #[allow(clippy::type_complexity)]
    fn mk_catalogs(
        entries: &[(&str, &[(&str, &str, &str)])],
    ) -> BTreeMap<String, BTreeMap<String, CatalogEntry>> {
        let mut out: BTreeMap<String, BTreeMap<String, CatalogEntry>> = BTreeMap::new();
        for (cat, pkgs) in entries {
            let mut inner = BTreeMap::new();
            for (pkg, spec, ver) in *pkgs {
                inner.insert(
                    (*pkg).to_string(),
                    CatalogEntry {
                        specifier: (*spec).to_string(),
                        version: (*ver).to_string(),
                    },
                );
            }
            out.insert((*cat).to_string(), inner);
        }
        out
    }

    fn mk_workspace_catalogs(
        entries: &[(&str, &[(&str, &str)])],
    ) -> BTreeMap<String, BTreeMap<String, String>> {
        entries
            .iter()
            .map(|(cat, pkgs)| {
                (
                    (*cat).to_string(),
                    pkgs.iter()
                        .map(|(p, s)| ((*p).to_string(), (*s).to_string()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn catalog_drift_fresh_when_specifiers_match() {
        let graph = LockfileGraph {
            catalogs: mk_catalogs(&[("default", &[("react", "^18.0.0", "18.2.0")])]),
            ..Default::default()
        };
        let ws = mk_workspace_catalogs(&[("default", &[("react", "^18.0.0")])]);
        assert_eq!(graph.check_catalogs_drift(&ws), DriftStatus::Fresh);
    }

    #[test]
    fn catalog_drift_stale_on_changed_specifier() {
        let graph = LockfileGraph {
            catalogs: mk_catalogs(&[("default", &[("react", "^18.0.0", "18.2.0")])]),
            ..Default::default()
        };
        let ws = mk_workspace_catalogs(&[("default", &[("react", "^19.0.0")])]);
        match graph.check_catalogs_drift(&ws) {
            DriftStatus::Stale { reason } => assert!(reason.contains("react")),
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn catalog_drift_fresh_when_workspace_adds_unused_entry() {
        // pnpm only writes referenced entries — an unreferenced
        // workspace entry is not drift. The "newly used" transition
        // is caught by the importer-level drift check.
        let graph = LockfileGraph::default();
        let ws = mk_workspace_catalogs(&[("default", &[("react", "^18")])]);
        assert_eq!(graph.check_catalogs_drift(&ws), DriftStatus::Fresh);
    }

    #[test]
    fn catalog_drift_stale_on_removed_workspace_entry() {
        let graph = LockfileGraph {
            catalogs: mk_catalogs(&[("default", &[("react", "^18", "18.2.0")])]),
            ..Default::default()
        };
        let ws = mk_workspace_catalogs(&[]);
        assert!(matches!(
            graph.check_catalogs_drift(&ws),
            DriftStatus::Stale { .. }
        ));
    }
}
