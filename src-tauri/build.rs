fn git_output(arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_bytes(arguments: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(arguments)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for &byte in *part {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Separador inequívoco entre status y diff.
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Incluye fuentes nuevas todavía no añadidas a Git sin convertir cachés,
/// vídeos de prueba o artefactos de benchmark en parte de la identidad del
/// binario. `git diff` no ve estos archivos y, sin este bloque, dos builds con
/// distinto `planetary_planner.rs` sin trackear compartirían fingerprint.
fn untracked_source_bytes() -> Vec<u8> {
    let raw = git_bytes(&[
        "-C",
        "..",
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ])
    .unwrap_or_default();
    let mut relative_paths = raw
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .filter(|path| {
            !path.starts_with("output/")
                && !path.starts_with("tmp/")
                && !path.starts_with("target/")
                && !path.starts_with("node_modules/")
        })
        .filter(|path| {
            std::path::Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "rs" | "js"
                            | "mjs"
                            | "json"
                            | "toml"
                            | "html"
                            | "css"
                            | "wgsl"
                            | "md"
                            | "yml"
                            | "yaml"
                    )
                })
        })
        .collect::<Vec<_>>();
    relative_paths.sort_unstable();

    let mut encoded = Vec::new();
    for relative in relative_paths {
        let path = std::path::Path::new("..").join(&relative);
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        encoded.extend_from_slice(relative.as_bytes());
        encoded.push(0);
        encoded.extend_from_slice(&metadata.len().to_le_bytes());
        // Los archivos de código/configuración deberían ser pequeños. El
        // límite evita que un JSON científico accidental de varios GiB haga
        // que cada `cargo check` lo lea completo; tamaño+ruta siguen separando
        // ese caso anómalo.
        if metadata.len() <= 16 * 1024 * 1024 {
            if let Ok(contents) = std::fs::read(path) {
                encoded.extend_from_slice(&contents);
            }
        }
        encoded.push(0xff);
    }
    encoded
}

fn main() {
    let commit = git_output(&["rev-parse", "HEAD"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let tracked_dirty = git_output(&["status", "--porcelain", "--untracked-files=no"])
        .map(|status| !status.is_empty())
        .unwrap_or(false);
    let diff = git_bytes(&[
        "-C",
        "..",
        "diff",
        "--binary",
        "--no-ext-diff",
        "HEAD",
        "--",
    ])
    .unwrap_or_default();
    let untracked_sources = untracked_source_bytes();
    let dirty = tracked_dirty || !untracked_sources.is_empty();
    let worktree_fingerprint = if diff.is_empty() && untracked_sources.is_empty() {
        "clean".to_string()
    } else {
        format!("fnv1a64-{:016x}", fnv1a64(&[&diff, &untracked_sources]))
    };
    println!("cargo:rustc-env=ZAS_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=ZAS_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=ZAS_WORKTREE_FINGERPRINT={worktree_fingerprint}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../src");
    println!("cargo:rerun-if-changed=../index.html");
    println!("cargo:rerun-if-changed=../package.json");
    tauri_build::build()
}
