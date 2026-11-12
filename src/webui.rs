//! The browser UI as static assets: embedded in the binary when it was built, else found on disk.

use std::path::{Path, PathBuf};

/// `(request path, bytes)` for each embedded file. Empty unless `web/pkg` existed at build time.
#[cfg(embedded_web)]
pub const EMBEDDED: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../web/index.html")),
    ("main.js", include_bytes!("../web/main.js")),
    ("pkg/emotibit.js", include_bytes!("../web/pkg/emotibit.js")),
    ("pkg/emotibit_bg.wasm", include_bytes!("../web/pkg/emotibit_bg.wasm")),
];
#[cfg(not(embedded_web))]
pub const EMBEDDED: &[(&str, &[u8])] = &[];

pub fn is_embedded() -> bool {
    !EMBEDDED.is_empty()
}

/// Look up an embedded file by its path relative to the site root.
pub fn embedded(path: &str) -> Option<&'static [u8]> {
    let path = if path.is_empty() { "index.html" } else { path };
    EMBEDDED.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

fn complete(dir: &Path) -> bool {
    dir.join("index.html").exists() && dir.join("pkg/emotibit_bg.wasm").exists()
}

/// Where to serve the UI from: `explicit`, else `$EMOTIBIT_WEB`, else the embedded copy
/// (`Ok(None)`), else `./web`, `./dist`, or `web/` next to the executable's project.
pub fn resolve(explicit: Option<PathBuf>) -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = explicit.or_else(|| std::env::var_os("EMOTIBIT_WEB").map(PathBuf::from)) {
        anyhow::ensure!(
            complete(&dir),
            "{} has no built UI (need index.html and pkg/emotibit_bg.wasm); run scripts/build-web.sh",
            dir.display()
        );
        return Ok(Some(dir));
    }
    if is_embedded() {
        return Ok(None);
    }
    let mut candidates = vec![PathBuf::from("web"), PathBuf::from("dist")];
    if let Ok(exe) = std::env::current_exe() {
        // target/{debug,release}/emotibit -> project root
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            candidates.push(root.join("web"));
            candidates.push(root.join("dist"));
        }
    }
    candidates
        .into_iter()
        .find(|d| complete(d))
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("no built web UI found. Run scripts/build-web.sh, then rebuild (it is embedded in the binary), or pass --web DIR"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_lookup_defaults_to_index() {
        if is_embedded() {
            assert!(embedded("").is_some());
            assert!(embedded("pkg/emotibit_bg.wasm").is_some_and(|b| b.starts_with(b"\0asm")));
        }
        assert!(embedded("../Cargo.toml").is_none());
    }

    #[test]
    fn explicit_dir_must_contain_a_build() {
        let dir = std::env::temp_dir().join(format!("emotibit-webui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(resolve(Some(dir.clone())).is_err());
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("index.html"), "x").unwrap();
        std::fs::write(dir.join("pkg/emotibit_bg.wasm"), "x").unwrap();
        assert_eq!(resolve(Some(dir.clone())).unwrap(), Some(dir.clone()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
