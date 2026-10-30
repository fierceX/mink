//! 构建脚本：自动构建 web 前端（npm run build）并把 dist 产物嵌入二进制。
//!
//! - `cargo build -p mink-server` 时自动执行 `npm run build`（apps/web 目录）
//! - 先复制到本次 OUT_DIR 的暂存目录并逐个验证可读，再发布为 `assets/`
//! - 遍历发布后的产物生成 `OUT_DIR/assets.rs` 清单（稳定排序）
//! - 资源按字节嵌入：前端产物包含字体等非 UTF-8 文件（KaTeX/Montserrat）
//! - npm 缺失/失败、空产物、复制/枚举/读取失败默认中止；
//!   `MINK_SERVER_ALLOW_EMPTY_WEB=1` 才允许确定的空 UI（不读取历史 dist）
//!
//! 所有失败都必须以非零退出结束：半成品清单一旦被本次编译接受，release
//! 二进制就会在运行时缺资源。

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Inputs the Vite build actually reads. `src` covers the app sources; the
/// rest are the entry HTML and the build configuration/lockfile, which change
/// the emitted bytes without touching `src`.
const WEB_INPUTS: &[&str] = &[
    "src",
    "index.html",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "tsconfig.json",
];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Add the operation and path to an I/O error so a failed build names the file.
fn context(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

/// One embedded file: its URL path (forward slashes) and published location.
struct Asset {
    rel: String,
    file: PathBuf,
}

fn reset_dir(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(context("remove", dir, error)),
    }
}

/// Enumerate every regular file below `dir`, following symlinks but rejecting
/// broken links, unsupported file types and directory cycles instead of
/// silently dropping them.
fn walk_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_into(dir, &mut Vec::new(), &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_into(dir: &Path, stack: &mut Vec<PathBuf>, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let canonical = fs::canonicalize(dir).map_err(|error| context("resolve", dir, error))?;
    if stack.contains(&canonical) {
        return Err(invalid(format!("directory cycle at {}", dir.display())));
    }
    stack.push(canonical);
    let entries = fs::read_dir(dir).map_err(|error| context("read_dir", dir, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| context("read_dir entry", dir, error))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| context("file type", &path, error))?;
        if file_type.is_dir() {
            walk_into(&path, stack, files)?;
        } else if file_type.is_file() {
            files.push(path);
        } else if file_type.is_symlink() {
            // Follow the link: the build product may legitimately reference a
            // real file, but a broken link is a failure, not a missing asset.
            let meta = fs::metadata(&path)
                .map_err(|error| invalid(format!("broken symlink {}: {error}", path.display())))?;
            if meta.is_dir() {
                walk_into(&path, stack, files)?;
            } else if meta.is_file() {
                files.push(path);
            } else {
                return Err(invalid(format!(
                    "unsupported file type at {}",
                    path.display()
                )));
            }
        } else {
            return Err(invalid(format!(
                "unsupported file type at {}",
                path.display()
            )));
        }
    }
    stack.pop();
    Ok(())
}

/// Copy the built dist into a staging directory under this OUT_DIR, verify every
/// file is readable, then publish it as `assets/` and return the manifest.
fn publish_dist(dist: &Path, out_dir: &Path) -> io::Result<Vec<Asset>> {
    let index = dist.join("index.html");
    let index_meta = fs::metadata(&index).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid("web build did not produce dist/index.html")
        } else {
            context("inspect", &index, error)
        }
    })?;
    if !index_meta.is_file() || index_meta.len() == 0 {
        return Err(invalid("dist/index.html is empty or not a file"));
    }

    let staging = out_dir.join("assets.staging");
    reset_dir(&staging)?;
    fs::create_dir_all(&staging).map_err(|error| context("create", &staging, error))?;

    let mut staged = Vec::new();
    for source in walk_files(dist)? {
        let rel_path = source
            .strip_prefix(dist)
            .map_err(|_| invalid(format!("{} is outside dist", source.display())))?
            .to_path_buf();
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        let dest = staging.join(&rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|error| context("create", parent, error))?;
        }
        fs::copy(&source, &dest).map_err(|error| context("copy", &source, error))?;
        // A staged file that cannot be opened would be an unreadable
        // include_bytes! input; catch it here, before the manifest exists.
        let staged_meta = fs::File::open(&dest)
            .and_then(|file| file.metadata())
            .map_err(|error| context("verify", &dest, error))?;
        let source_len = fs::metadata(&source)
            .map_err(|error| context("inspect", &source, error))?
            .len();
        if !staged_meta.is_file() || staged_meta.len() != source_len {
            return Err(invalid(format!(
                "staged {} does not match the built asset",
                dest.display()
            )));
        }
        staged.push((rel, rel_path));
    }
    if !staging.join("index.html").is_file() {
        return Err(invalid("staged assets are missing index.html"));
    }

    let published = out_dir.join("assets");
    reset_dir(&published)?;
    fs::rename(&staging, &published).map_err(|error| context("publish", &staging, error))?;

    Ok(staged
        .into_iter()
        .map(|(rel, rel_path)| Asset {
            rel,
            file: published.join(rel_path),
        })
        .collect())
}

/// Write the include_bytes! manifest with stable ordering and single escaping.
fn write_manifest(out_dir: &Path, assets: &[Asset]) -> io::Result<()> {
    let mut entries = String::new();
    for asset in assets {
        let path = asset.file.to_str().ok_or_else(|| {
            invalid(format!(
                "asset path is not valid UTF-8: {}",
                asset.file.display()
            ))
        })?;
        entries.push_str(&format!(
            "    ({:?}, include_bytes!({:?})),\n",
            asset.rel, path
        ));
    }
    let asset_rs = format!(
        "// @generated by build.rs — web dist 嵌入清单（勿手改）\npub static FILES: &[(&str, &[u8])] = &[\n{entries}];\n"
    );
    fs::write(out_dir.join("assets.rs"), asset_rs)
        .map_err(|error| context("write", &out_dir.join("assets.rs"), error))
}

fn run_npm(web_dir: &Path) -> Result<(), String> {
    Command::new("npm")
        .args(["run", "build"])
        .current_dir(web_dir)
        .status()
        .map_err(|error| {
            format!(
                "could not run `npm run build` in {}: {error}",
                web_dir.display()
            )
        })
        .and_then(|status| {
            status.success().then_some(()).ok_or_else(|| {
                format!("`npm run build` failed in {} ({status})", web_dir.display())
            })
        })
}

fn main() {
    for input in WEB_INPUTS {
        println!("cargo:rerun-if-changed=../../apps/web/{input}");
    }

    // ── 版本溯源：嵌入 git commit hash（与 mink-cli/build.rs 同一协议）──
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let git_dir = manifest.join("../../.git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("refs/heads/main").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    println!("cargo:rerun-if-env-changed=MINK_GIT_HASH");
    println!("cargo:rerun-if-env-changed=MINK_SERVER_ALLOW_EMPTY_WEB");
    let hash = match env::var("MINK_GIT_HASH") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => git_short_hash()
            .map(|hash| {
                if working_tree_dirty() {
                    format!("{hash}-dirty")
                } else {
                    hash
                }
            })
            .unwrap_or_default(),
    };
    println!("cargo:rustc-env=MINK_GIT_HASH={hash}");

    // The served frontend is the React workbench in apps/web; its dist is
    // embedded here (or served from disk with MINK_SERVER_DEV_WEB=<dir>/dist).
    let web_dir = manifest.join("../../apps/web");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let allow_empty = env::var("MINK_SERVER_ALLOW_EMPTY_WEB").as_deref() == Ok("1");

    let built = run_npm(&web_dir).and_then(|()| {
        publish_dist(&web_dir.join("dist"), &out_dir)
            .map_err(|error| format!("failed to publish the built web UI: {error}"))
    });
    let assets = match built {
        Ok(assets) => assets,
        Err(reason) if allow_empty => {
            // Deterministic empty UI: remove anything an earlier build left so
            // a stale dist can never be embedded by accident.
            reset_dir(&out_dir.join("assets"))
                .and_then(|()| reset_dir(&out_dir.join("assets.staging")))
                .unwrap_or_else(|error| {
                    panic!("[build.rs] cannot clear stale web assets: {error}")
                });
            eprintln!(
                "[build.rs] warning: {reason}; building with an empty embedded web UI by explicit opt-in"
            );
            Vec::new()
        }
        Err(reason) => panic!(
            "[build.rs] {reason}\nset MINK_SERVER_ALLOW_EMPTY_WEB=1 only for explicit headless builds"
        ),
    };

    write_manifest(&out_dir, &assets).unwrap_or_else(|error| panic!("[build.rs] {error}"));
    println!("[build.rs] embedded {} web assets", assets.len());
}

fn git_short_hash() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!hash.is_empty()).then_some(hash)
}

/// Dirty = any tracked file changed (untracked build artifacts excluded).
fn working_tree_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty())
}
