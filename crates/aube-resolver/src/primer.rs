use aube_manifest::BundledDependencies;
use aube_registry::{Attestations, Dist, NpmUser, Packument, PeerDepMeta, VersionMetadata};
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const PRIMER_FORMAT: &str = "rkyv-v1";
const PRUNE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const AUTO_PRUNE_DENOMINATOR: u8 = 100;

#[derive(Default)]
pub struct PruneStats {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
pub(crate) struct Seed {
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    packument: PrimerPackument,
}

impl Seed {
    pub(crate) fn packument(&self) -> Packument {
        self.packument.to_packument()
    }
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
struct PrimerPackument {
    name: String,
    modified: Option<String>,
    dist_tags: BTreeMap<String, String>,
    versions: Vec<PrimerVersion>,
}

impl PrimerPackument {
    fn to_packument(&self) -> Packument {
        let mut time = BTreeMap::new();
        let versions = self
            .versions
            .iter()
            .map(|v| {
                if let Some(published_at) = v.published_at.as_ref() {
                    time.insert(v.version.clone(), published_at.clone());
                }
                (
                    v.version.clone(),
                    v.metadata.to_version_metadata(&self.name, &v.version),
                )
            })
            .collect();
        Packument {
            name: self.name.clone(),
            modified: self.modified.clone(),
            versions,
            dist_tags: self.dist_tags.clone(),
            time,
        }
    }
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
struct PrimerVersion {
    version: String,
    published_at: Option<String>,
    metadata: PrimerVersionMetadata,
}

#[derive(Archive, Clone, Default, RkyvSerialize, RkyvDeserialize)]
struct PrimerVersionMetadata {
    dependencies: BTreeMap<String, String>,
    peer_dependencies: BTreeMap<String, String>,
    peer_dependencies_meta: BTreeMap<String, PrimerPeerDepMeta>,
    optional_dependencies: BTreeMap<String, String>,
    bundled_dependencies: Option<PrimerBundledDependencies>,
    dist: Option<PrimerDist>,
    os: Vec<String>,
    cpu: Vec<String>,
    libc: Vec<String>,
    engines: BTreeMap<String, String>,
    license: Option<String>,
    funding_url: Option<String>,
    bin: BTreeMap<String, String>,
    has_install_script: bool,
    deprecated: Option<String>,
    trusted_publisher: bool,
}

impl PrimerVersionMetadata {
    fn to_version_metadata(&self, name: &str, version: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_owned(),
            version: version.to_owned(),
            dependencies: self.dependencies.clone(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: self.peer_dependencies.clone(),
            peer_dependencies_meta: self
                .peer_dependencies_meta
                .iter()
                .map(|(name, meta)| (name.clone(), meta.to_peer_dep_meta()))
                .collect(),
            optional_dependencies: self.optional_dependencies.clone(),
            bundled_dependencies: self
                .bundled_dependencies
                .as_ref()
                .map(PrimerBundledDependencies::to_bundled_dependencies),
            dist: self.dist.as_ref().map(PrimerDist::to_dist),
            os: self.os.clone(),
            cpu: self.cpu.clone(),
            libc: self.libc.clone(),
            engines: self.engines.clone(),
            license: self.license.clone(),
            funding_url: self.funding_url.clone(),
            bin: self.bin.clone(),
            has_install_script: self.has_install_script,
            deprecated: self.deprecated.clone(),
            npm_user: self.trusted_publisher.then(|| NpmUser {
                trusted_publisher: Some(serde_json::json!({"id": "npm-primer"})),
            }),
        }
    }
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
struct PrimerPeerDepMeta {
    optional: bool,
}

impl PrimerPeerDepMeta {
    fn to_peer_dep_meta(&self) -> PeerDepMeta {
        PeerDepMeta {
            optional: self.optional,
        }
    }
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
enum PrimerBundledDependencies {
    List(Vec<String>),
    All(bool),
}

impl PrimerBundledDependencies {
    fn to_bundled_dependencies(&self) -> BundledDependencies {
        match self {
            Self::List(v) => BundledDependencies::List(v.clone()),
            Self::All(v) => BundledDependencies::All(*v),
        }
    }
}

#[derive(Archive, Clone, RkyvSerialize, RkyvDeserialize)]
struct PrimerDist {
    tarball: String,
    integrity: Option<String>,
    shasum: Option<String>,
    provenance: bool,
}

impl PrimerDist {
    fn to_dist(&self) -> Dist {
        Dist {
            tarball: self.tarball.clone(),
            integrity: self.integrity.clone(),
            shasum: self.shasum.clone(),
            attestations: self.provenance.then(|| Attestations {
                provenance: Some(serde_json::json!({
                    "predicateType": "https://slsa.dev/provenance/v1"
                })),
            }),
        }
    }
}

static PRIMER: OnceLock<Option<FxHashMap<String, Seed>>> = OnceLock::new();

pub(crate) fn get(name: &str) -> Option<Seed> {
    PRIMER
        .get_or_init(load)
        .as_ref()
        .and_then(|primer| primer.get(name).cloned())
}

fn load() -> Option<FxHashMap<String, Seed>> {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/primer.rkyv.zst"));
    if bytes.is_empty() {
        return Some(FxHashMap::default());
    }
    let archived = extracted_primer(bytes)
        .and_then(|path| std::fs::read(path).ok())
        .or_else(|| zstd::stream::decode_all(Cursor::new(bytes)).ok())?;
    let primer = rkyv::from_bytes::<BTreeMap<String, Seed>, rkyv::rancor::Error>(&archived).ok()?;
    Some(primer.into_iter().collect())
}

fn extracted_primer(compressed: &[u8]) -> Option<PathBuf> {
    let dir = primer_cache_dir()?;
    let filename = current_filename(compressed);
    let path = dir.join(&filename);
    auto_prune(&dir, &filename);
    if path.is_file() {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!(".{filename}.tmp-{}", std::process::id()));
    let decoded = zstd::stream::decode_all(Cursor::new(compressed)).ok()?;
    if let Err(e) = write_atomically(&tmp, &path, &decoded) {
        tracing::debug!(
            "failed to extract bundled primer to {}: {e}",
            path.display()
        );
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(path)
}

fn write_atomically(tmp: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut file = std::fs::File::create(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(_) if path.is_file() => {
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn auto_prune(dir: &Path, current_filename: &str) {
    if !random_byte().is_multiple_of(AUTO_PRUNE_DENOMINATOR) {
        return;
    }
    if let Err(e) = prune_old(dir, current_filename, PRUNE_AGE, false, true) {
        tracing::debug!("failed to prune old primer cache files: {e}");
    }
}

pub fn prune_cache(dry_run: bool, age: Duration) -> std::io::Result<PruneStats> {
    let Some(dir) = primer_cache_dir() else {
        return Ok(PruneStats::default());
    };
    let current = current_filename(include_bytes!(concat!(env!("OUT_DIR"), "/primer.rkyv.zst")));
    prune_old(&dir, &current, age, dry_run, false)
}

fn prune_old(
    dir: &Path,
    current_filename: &str,
    age: Duration,
    dry_run: bool,
    check_sentinel: bool,
) -> std::io::Result<PruneStats> {
    let mut stats = PruneStats::default();
    std::fs::create_dir_all(dir)?;
    let sentinel = dir.join(".auto_prune");
    if check_sentinel
        && let Ok(modified) = sentinel.metadata().and_then(|m| m.modified())
        && modified.elapsed().unwrap_or_default() < age
    {
        return Ok(stats);
    }
    if check_sentinel {
        touch(&sentinel)?;
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == current_filename || !is_primer_cache_file(name) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.modified()?.elapsed().unwrap_or_default() > age {
            stats.files += 1;
            stats.bytes += metadata.len();
            if !dry_run {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(stats)
}

fn touch(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(b"\n")
}

fn is_primer_cache_file(name: &str) -> bool {
    name.starts_with(&format!("{PRIMER_FORMAT}-")) && name.ends_with(".json")
}

fn random_byte() -> u8 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    (nanos as u8) ^ (std::process::id() as u8)
}

fn current_filename(compressed: &[u8]) -> String {
    let hash = blake3::hash(compressed).to_hex().to_string();
    format!("{PRIMER_FORMAT}-{hash}.json")
}

fn primer_cache_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("AUBE_CACHE_DIR") {
        return Some(PathBuf::from(base).join("primer"));
    }
    cache_base_dir().map(|p| p.join("aube").join("primer"))
}

#[cfg(unix)]
fn cache_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
}

#[cfg(windows)]
fn cache_base_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_primer_loads() {
        assert!(super::PRIMER.get_or_init(super::load).is_some());
    }

    #[test]
    fn primer_cache_file_match_is_narrow() {
        assert!(is_primer_cache_file("rkyv-v1-abc.json"));
        assert!(!is_primer_cache_file(".auto_prune"));
        assert!(!is_primer_cache_file("rkyv-v1-abc.tmp"));
        assert!(!is_primer_cache_file("other-v1-abc.json"));
    }

    #[test]
    fn prune_keeps_current_and_removes_old_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::fs::write(dir.join("rkyv-v1-current.json"), "{}").unwrap();
        std::fs::write(dir.join("rkyv-v1-old.json"), "{}").unwrap();
        std::fs::write(dir.join("packument.json"), "{}").unwrap();
        let stats = prune_old(
            dir,
            "rkyv-v1-current.json",
            Duration::from_secs(0),
            false,
            false,
        )
        .unwrap();
        assert_eq!(stats.files, 1);
        assert!(dir.join("rkyv-v1-current.json").exists());
        assert!(!dir.join("rkyv-v1-old.json").exists());
        assert!(dir.join("packument.json").exists());
    }
}
