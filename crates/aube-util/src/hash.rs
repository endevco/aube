use std::hash::{Hash, Hasher};

pub fn ordered_seq_hash<I, T>(iter: I) -> u64
where
    I: IntoIterator<Item = T>,
    T: Hash,
    I::IntoIter: ExactSizeIterator,
{
    let iter = iter.into_iter();
    let mut h = rustc_hash::FxHasher::default();
    iter.len().hash(&mut h);
    for item in iter {
        item.hash(&mut h);
    }
    h.finish()
}

pub fn meta_hash<'a, I, S>(packages: I, scripts: S) -> [u8; 32]
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
    S: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aube-meta-v1\npackages\n");
    for (name, version) in packages {
        hasher.update(name.as_bytes());
        hasher.update(b"@");
        hasher.update(version.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"scripts\n");
    for (name, body) in scripts {
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(body.as_bytes());
        hasher.update(b"\n");
    }
    *hasher.finalize().as_bytes()
}

pub const INSTALL_SHAPE_FIELDS: &[&str] = &[
    "aube",
    "bundleDependencies",
    "bundledDependencies",
    "catalog",
    "catalogs",
    "dependencies",
    "devDependencies",
    "engines",
    "name",
    "optionalDependencies",
    "overrides",
    "peerDependencies",
    "peerDependenciesMeta",
    "pnpm",
    "publishConfig",
    "resolutions",
    "version",
    "workspaces",
];

pub fn manifest_install_shape_digest(manifest: &serde_json::Value) -> [u8; 32] {
    let obj = match manifest.as_object() {
        Some(o) => o,
        None => return *blake3::hash(b"aube-manifest-v1/not-an-object").as_bytes(),
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aube-manifest-v1\n");
    for field in INSTALL_SHAPE_FIELDS {
        if let Some(v) = obj.get(*field) {
            hasher.update(field.as_bytes());
            hasher.update(b"=");
            canonical_json(v, &mut hasher);
            hasher.update(b"\n");
        }
    }
    *hasher.finalize().as_bytes()
}

fn canonical_json(v: &serde_json::Value, hasher: &mut blake3::Hasher) {
    use serde_json::Value;
    match v {
        Value::Null => {
            hasher.update(b"null");
        }
        Value::Bool(b) => {
            hasher.update(if *b { b"true" } else { b"false" });
        }
        Value::Number(n) => {
            hasher.update(n.to_string().as_bytes());
        }
        Value::String(s) => {
            hasher.update(b"\"");
            hasher.update(s.as_bytes());
            hasher.update(b"\"");
        }
        Value::Array(items) => {
            hasher.update(b"[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    hasher.update(b",");
                }
                canonical_json(item, hasher);
            }
            hasher.update(b"]");
        }
        Value::Object(obj) => {
            hasher.update(b"{");
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    hasher.update(b",");
                }
                hasher.update(b"\"");
                hasher.update(k.as_bytes());
                hasher.update(b"\":");
                if let Some(val) = obj.get(*k) {
                    canonical_json(val, hasher);
                }
            }
            hasher.update(b"}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_seq_hash_is_order_sensitive() {
        let a = ordered_seq_hash(["a", "b", "c"].iter().copied());
        let b = ordered_seq_hash(["c", "b", "a"].iter().copied());
        assert_ne!(a, b);
    }

    #[test]
    fn ordered_seq_hash_detects_count_changes() {
        let short = ordered_seq_hash(["a", "b"].iter().copied());
        let long = ordered_seq_hash(["a", "b", "c"].iter().copied());
        assert_ne!(short, long);
    }

    #[test]
    fn meta_hash_stable_for_same_inputs() {
        let pkgs = [("react", "19.0.0"), ("next", "15.1.3")];
        let scripts: [(&str, &str); 0] = [];
        let a = meta_hash(pkgs.iter().copied(), scripts.iter().copied());
        let b = meta_hash(pkgs.iter().copied(), scripts.iter().copied());
        assert_eq!(a, b);
    }

    #[test]
    fn manifest_digest_ignores_scripts_and_license() {
        let a: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","version":"1.0.0","dependencies":{"react":"19.0.0"},"scripts":{"test":"vitest"},"license":"MIT"}"#,
        )
        .unwrap();
        let b: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","version":"1.0.0","dependencies":{"react":"19.0.0"},"scripts":{"test":"jest --watch"},"license":"Apache-2.0"}"#,
        )
        .unwrap();
        assert_eq!(
            manifest_install_shape_digest(&a),
            manifest_install_shape_digest(&b)
        );
    }

    #[test]
    fn manifest_digest_reacts_to_dep_change() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"dependencies":{"react":"19.0.0"}}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"dependencies":{"react":"19.1.0"}}"#).unwrap();
        assert_ne!(
            manifest_install_shape_digest(&a),
            manifest_install_shape_digest(&b)
        );
    }

    #[test]
    fn manifest_digest_stable_under_key_reorder() {
        let a: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","dependencies":{"b":"1","a":"2"},"devDependencies":{"c":"3"}}"#,
        )
        .unwrap();
        let b: serde_json::Value = serde_json::from_str(
            r#"{"devDependencies":{"c":"3"},"dependencies":{"a":"2","b":"1"},"name":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            manifest_install_shape_digest(&a),
            manifest_install_shape_digest(&b)
        );
    }

    #[test]
    fn meta_hash_reacts_to_script_change() {
        let pkgs = [("react", "19.0.0")];
        let s1 = [("build", "tsc")];
        let s2 = [("build", "tsc --watch")];
        let a = meta_hash(pkgs.iter().copied(), s1.iter().copied());
        let b = meta_hash(pkgs.iter().copied(), s2.iter().copied());
        assert_ne!(a, b);
    }
}
