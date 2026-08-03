use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    emit_git_commit();

    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_icon("assets/stk-icon.ico")
            .compile()
            .expect("failed to embed the SSH Tunnel Keeper application icon");
    }
}

fn emit_git_commit() {
    println!("cargo:rerun-if-env-changed=STK_GIT_COMMIT");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let repository_root = manifest_dir.join("../..");
    track_git_head(&repository_root);

    let commit = env::var("STK_GIT_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
        .or_else(|| git_output(&repository_root, &["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=STK_GIT_COMMIT={commit}");
}

fn track_git_head(repository_root: &Path) {
    let Some(git_dir) = git_output(repository_root, &["rev-parse", "--git-dir"]) else {
        return;
    };
    let git_dir = {
        let path = PathBuf::from(git_dir);
        if path.is_absolute() {
            path
        } else {
            repository_root.join(path)
        }
    };
    let head_path = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );

    if let Ok(head) = fs::read_to_string(&head_path)
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(reference).display()
        );
    }
}

fn git_output(repository_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repository_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
