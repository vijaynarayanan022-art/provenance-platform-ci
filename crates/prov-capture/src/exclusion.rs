//! Deterministic file eligibility rules. Changing a rule or reason code requires a new ID.

pub const CLASSIFIER_ID: &str = "exclusion-v1";
pub const FILE_SIZE_LIMIT_REASON: &str = "FILE_SIZE_LIMIT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exclusion {
    pub reason_code: &'static str,
    pub limiting_basis: Option<&'static str>,
}

pub fn classify_path(path: &str) -> Option<Exclusion> {
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let parts = lower.split('/').collect::<Vec<_>>();
    let base = parts.last().copied().unwrap_or_default();

    if base.starts_with(".env")
        || [".pem", ".key", ".pfx"]
            .iter()
            .any(|ext| base.ends_with(ext))
        || base.starts_with("id_rsa")
        || base.starts_with("id_ed25519")
        || base.contains("credentials")
        || base.contains("secret")
    {
        return Some(exclusion("SENSITIVE_FILE"));
    }
    if base.ends_with(".pyc") || base.ends_with(".pyo") || parts.contains(&"__pycache__") {
        return Some(exclusion("GENERATED_BYTECODE"));
    }
    if parts
        .iter()
        .any(|part| matches!(*part, "node_modules" | ".venv"))
    {
        return Some(exclusion("DEPENDENCY_DIRECTORY"));
    }
    if parts
        .iter()
        .any(|part| matches!(*part, "target" | "dist" | "build"))
    {
        return Some(exclusion("BUILD_OUTPUT_DIRECTORY"));
    }
    if parts.iter().any(|part| {
        matches!(
            *part,
            "vendor" | "vendors" | "third_party" | "third-party" | "bower_components"
        )
    }) {
        return Some(exclusion("VENDORED_DIRECTORY"));
    }
    if is_lockfile(base) {
        return Some(exclusion("LOCKFILE"));
    }
    if base.ends_with(".map") {
        return Some(exclusion("SOURCE_MAP"));
    }
    if is_minified(base) {
        return Some(exclusion("MINIFIED_FILE"));
    }
    if is_binary_extension(base) {
        return Some(exclusion("BINARY_FILE"));
    }
    None
}

pub fn classify(path: &str, content: &[u8]) -> Option<Exclusion> {
    let path_exclusion = classify_path(path);
    if path_exclusion.is_some_and(|exclusion| exclusion.reason_code != "BINARY_FILE") {
        return path_exclusion;
    }
    classify_content(content).or(path_exclusion)
}

pub fn classify_content(content: &[u8]) -> Option<Exclusion> {
    if content.iter().take(8192).any(|byte| *byte == 0) {
        Some(Exclusion {
            reason_code: "BINARY_CONTENT",
            limiting_basis: Some("CONTENT_RESTRICTED"),
        })
    } else if has_generated_header(content) {
        Some(exclusion("GENERATED_HEADER"))
    } else {
        None
    }
}

fn exclusion(reason_code: &'static str) -> Exclusion {
    Exclusion {
        reason_code,
        limiting_basis: None,
    }
}

fn is_lockfile(base: &str) -> bool {
    base.ends_with(".lock")
        || matches!(
            base,
            "npm-shrinkwrap.json"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "bun.lockb"
                | "cargo.lock"
                | "composer.lock"
                | "gemfile.lock"
                | "gopkg.lock"
                | "pipfile.lock"
                | "poetry.lock"
                | "pubspec.lock"
        )
}

fn is_minified(base: &str) -> bool {
    let Some((stem, extension)) = base.rsplit_once('.') else {
        return false;
    };
    matches!(extension, "js" | "mjs" | "cjs" | "css") && stem.ends_with(".min")
}

fn is_binary_extension(base: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "7z", "a", "aac", "avi", "avif", "bin", "bmp", "bz2", "class", "dll", "dylib", "eot",
        "exe", "flac", "gif", "gz", "heic", "heif", "ico", "jpeg", "jpg", "lib", "m4a", "mkv",
        "mov", "mp3", "mp4", "mpeg", "mpg", "o", "ogg", "otf", "pdf", "png", "rar", "raw", "so",
        "tar", "tif", "tiff", "ttf", "wav", "wasm", "webm", "webp", "woff", "woff2", "xz", "zip",
        "zst",
    ];
    base.rsplit_once('.')
        .is_some_and(|(_, extension)| EXTENSIONS.contains(&extension))
}

fn has_generated_header(content: &[u8]) -> bool {
    String::from_utf8_lossy(content)
        .lines()
        .take(5)
        .any(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("code generated")
                || line.contains("@generated")
                || line.contains("do not edit")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_covers_each_path_rule_with_stable_reasons() {
        let cases = [
            ("__pycache__/module.py", "GENERATED_BYTECODE"),
            ("module.pyc", "GENERATED_BYTECODE"),
            ("module.pyo", "GENERATED_BYTECODE"),
            ("node_modules/pkg/index.js", "DEPENDENCY_DIRECTORY"),
            (".venv/lib/module.py", "DEPENDENCY_DIRECTORY"),
            ("target/debug/module.rs", "BUILD_OUTPUT_DIRECTORY"),
            ("dist/app.js", "BUILD_OUTPUT_DIRECTORY"),
            ("build/output.c", "BUILD_OUTPUT_DIRECTORY"),
            ("Cargo.lock", "LOCKFILE"),
            ("package-lock.json", "LOCKFILE"),
            ("assets/app.min.js", "MINIFIED_FILE"),
            ("assets/app.min.css", "MINIFIED_FILE"),
            ("assets/app.js.map", "SOURCE_MAP"),
            ("images/photo.png", "BINARY_FILE"),
            ("images/photo.heic", "BINARY_FILE"),
            ("fonts/body.woff2", "BINARY_FILE"),
            ("archives/source.tar.gz", "BINARY_FILE"),
            ("archives/source.7z", "BINARY_FILE"),
            ("documents/report.pdf", "BINARY_FILE"),
            ("media/movie.mp4", "BINARY_FILE"),
            ("compiled/module.class", "BINARY_FILE"),
            ("vendor/library/src.rs", "VENDORED_DIRECTORY"),
            ("third_party/library/src.rs", "VENDORED_DIRECTORY"),
            ("third-party/library/src.rs", "VENDORED_DIRECTORY"),
            ("vendors/library/src.rs", "VENDORED_DIRECTORY"),
            ("bower_components/library/src.js", "VENDORED_DIRECTORY"),
            (".env.production", "SENSITIVE_FILE"),
            ("secrets/credentials.json", "SENSITIVE_FILE"),
            ("package.json.lock", "LOCKFILE"),
            ("pnpm-lock.yaml", "LOCKFILE"),
            ("yarn.lock", "LOCKFILE"),
            ("app.min.mjs", "MINIFIED_FILE"),
            ("app.min.cjs", "MINIFIED_FILE"),
        ];
        for (path, reason) in cases {
            assert_eq!(
                classify_path(path)
                    .unwrap_or_else(|| panic!("expected exclusion for {path}"))
                    .reason_code,
                reason,
                "{path}"
            );
        }
        assert!(classify_path("src/ordinary.rs").is_none());
    }

    #[test]
    fn generated_headers_and_binary_content_have_stable_reasons() {
        for header in [
            b"// Code generated by tool. DO NOT EDIT.\n".as_slice(),
            b"# @generated\n".as_slice(),
            b"# DO NOT EDIT\n".as_slice(),
        ] {
            assert_eq!(
                classify("src/generated.rs", header).unwrap().reason_code,
                "GENERATED_HEADER"
            );
        }
        let binary = classify("data.dat", b"text\0binary").unwrap();
        assert_eq!(binary.reason_code, "BINARY_CONTENT");
        assert_eq!(binary.limiting_basis, Some("CONTENT_RESTRICTED"));
        assert_eq!(
            classify("vendor/library.rs", b"// @generated\n")
                .unwrap()
                .reason_code,
            "VENDORED_DIRECTORY"
        );
        assert_eq!(
            classify("images/photo.png", b"// @generated\n")
                .unwrap()
                .reason_code,
            "GENERATED_HEADER"
        );
        assert!(classify("src/ordinary.rs", b"fn ordinary() {}\n").is_none());
    }

    #[test]
    fn generated_headers_are_checked_in_the_first_five_lines() {
        let content = b"// generated by a fixture\n// line two\n// line three\n// line four\n// DO NOT EDIT\nfn generated() {}\n";
        assert_eq!(
            classify("src/generated.rs", content).unwrap().reason_code,
            "GENERATED_HEADER"
        );
        let later_header =
            b"// line one\n// line two\n// line three\n// line four\n// line five\n// @generated\n";
        assert!(classify("src/ordinary.rs", later_header).is_none());
    }
}
