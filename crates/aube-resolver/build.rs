use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEV_TOP: usize = 100;
const RELEASE_TOP: usize = 2000;

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Deserialize)]
struct Seed {
    #[serde(default, rename = "e")]
    etag: Option<String>,
    #[serde(default, rename = "lm")]
    last_modified: Option<String>,
    #[serde(rename = "p")]
    packument: PrimerPackument,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Deserialize)]
struct PrimerPackument {
    #[serde(rename = "n")]
    name: String,
    #[serde(default, rename = "m")]
    modified: Option<String>,
    #[serde(default, rename = "d")]
    dist_tags: BTreeMap<String, String>,
    #[serde(default, rename = "v")]
    versions: Vec<PrimerVersion>,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Deserialize)]
struct PrimerVersion {
    #[serde(rename = "v")]
    version: String,
    #[serde(default, rename = "t")]
    published_at: Option<String>,
    #[serde(default, rename = "m")]
    metadata: PrimerVersionMetadata,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Default, Deserialize)]
struct PrimerVersionMetadata {
    #[serde(default, rename = "d")]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "p")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "pm")]
    peer_dependencies_meta: BTreeMap<String, PrimerPeerDepMeta>,
    #[serde(default, rename = "o")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "b")]
    bundled_dependencies: Option<PrimerBundledDependencies>,
    #[serde(default, rename = "dt")]
    dist: Option<PrimerDist>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
    #[serde(default, rename = "e")]
    engines: BTreeMap<String, String>,
    #[serde(default, rename = "l")]
    license: Option<String>,
    #[serde(default, rename = "f")]
    funding_url: Option<String>,
    #[serde(default)]
    bin: BTreeMap<String, String>,
    #[serde(default, rename = "h")]
    has_install_script: bool,
    #[serde(default, rename = "x")]
    deprecated: Option<String>,
    #[serde(default, rename = "u")]
    trusted_publisher: bool,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Deserialize)]
struct PrimerPeerDepMeta {
    #[serde(default)]
    optional: bool,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Deserialize)]
#[serde(untagged)]
enum PrimerBundledDependencies {
    List(Vec<String>),
    All(bool),
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Deserialize)]
struct PrimerDist {
    #[serde(rename = "t")]
    tarball: String,
    #[serde(default, rename = "i")]
    integrity: Option<String>,
    #[serde(default, rename = "s")]
    shasum: Option<String>,
    #[serde(default, rename = "a")]
    provenance: bool,
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let source = std::env::var_os("AUBE_PRIMER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let top = primer_top();
            manifest_dir
                .join("data")
                .join(format!("primer-top{top}.rkyv.zst"))
        });

    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_PATH");
    println!("cargo:rerun-if-env-changed=AUBE_PRIMER_TOP");
    println!("cargo:rerun-if-changed={}", source.display());

    if !source.is_file() {
        if std::env::var_os("AUBE_PRIMER_PATH").is_some() {
            panic!(
                "AUBE_PRIMER_PATH does not point to a file: {}",
                source.display()
            );
        }
        generate(&manifest_dir, &source, primer_top());
    }

    let generated_at = std::fs::metadata(&source)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
        })
        .as_secs();
    println!("cargo:rustc-env=AUBE_PRIMER_GENERATED_AT={generated_at}");

    let bytes = std::fs::read(&source)
        .unwrap_or_else(|e| panic!("failed to read primer {}: {e}", source.display()));
    write_package_blob(&out_dir, &bytes);
}

fn primer_top() -> usize {
    if let Some(top) = std::env::var_os("AUBE_PRIMER_TOP") {
        return top
            .to_string_lossy()
            .parse()
            .expect("AUBE_PRIMER_TOP must be a positive integer");
    }
    match std::env::var("PROFILE").as_deref() {
        Ok("release" | "release-native" | "release-pgo") => RELEASE_TOP,
        _ => DEV_TOP,
    }
}

fn generate(manifest_dir: &Path, source: &Path, top: usize) {
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("resolver crate lives under crates/aube-resolver");
    let json = source.with_extension("json");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();

    let status = Command::new("node")
        .arg(workspace.join("scripts/generate-primer.mjs"))
        .arg("--top")
        .arg(top.to_string())
        .arg("--versions")
        .arg("all")
        .arg("--out")
        .arg(&json)
        .status()
        .expect("failed to run scripts/generate-primer.mjs");
    assert!(status.success(), "scripts/generate-primer.mjs failed");

    let input = std::fs::read(&json).unwrap();
    let primer: BTreeMap<String, Seed> = serde_json::from_slice(&input).unwrap();
    let archived = rkyv::to_bytes::<rkyv::rancor::Error>(&primer).unwrap();
    let compressed = zstd::stream::encode_all(Cursor::new(archived), 19).unwrap();
    std::fs::write(source, compressed).unwrap();
    let _ = std::fs::remove_file(json);
}

fn write_package_blob(out_dir: &Path, compressed: &[u8]) {
    let mut blob = Vec::new();
    let mut index = Vec::new();
    if !compressed.is_empty() {
        let archived = zstd::stream::decode_all(Cursor::new(compressed)).unwrap();
        let primer =
            rkyv::from_bytes::<BTreeMap<String, Seed>, rkyv::rancor::Error>(&archived).unwrap();
        for (name, seed) in primer {
            let archived = rkyv::to_bytes::<rkyv::rancor::Error>(&seed).unwrap();
            let compressed = zstd::stream::encode_all(Cursor::new(archived), 10).unwrap();
            let offset = blob.len();
            let len = compressed.len();
            blob.extend_from_slice(&compressed);
            index.push((name, offset, len));
        }
    }
    std::fs::write(out_dir.join("primer-packages.bin"), blob).unwrap();

    let mut generated =
        "static PRIMER_BLOB: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/primer-packages.bin\"));\nstatic PRIMER_INDEX: &[(&str, usize, usize)] = &[\n"
            .to_string();
    for (name, offset, len) in index {
        generated.push_str(&format!("    ({name:?}, {offset}, {len}),\n"));
    }
    generated.push_str("];\n");
    std::fs::write(out_dir.join("primer_index.rs"), generated).unwrap();
}
