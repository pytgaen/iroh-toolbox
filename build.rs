use std::{env, fmt::Write, fs, path::PathBuf, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=Cargo.toml");
    // Follow both symbolic and detached HEADs, including linked worktrees.
    let mut git_paths = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        git_paths.push(branch);
    }
    for path in git_paths {
        if let Some(path) = git(&["rev-parse", "--git-path", &path])
            && std::path::Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let commit = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or("unknown".to_owned());
    let mut version = format!(
        "v{}\n  Commit    {commit}\n  Platform  {}\n  License   {}\n\n  Components\n",
        env::var("CARGO_PKG_VERSION").unwrap(),
        env::var("TARGET").unwrap(),
        env::var("CARGO_PKG_LICENSE").unwrap(),
    );
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string("Cargo.lock").expect("read Cargo.lock"))
            .expect("parse Cargo.lock");
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string("Cargo.toml").expect("read Cargo.toml"))
            .expect("parse Cargo.toml");
    let dependencies = manifest["dependencies"].as_table().expect("dependencies");
    for package in lock["package"].as_array().expect("Cargo.lock packages") {
        let name = package["name"].as_str().expect("package name");
        // iroh components and the PAKE used by code pairing.
        let listed = name == "iroh" || name.starts_with("iroh-") || name == "spake2";
        if listed && dependencies.contains_key(name) {
            let number = package["version"].as_str().expect("package version");
            writeln!(version, "    {name:<14}  {number}").unwrap();
        }
    }
    // Windows gives the main thread 1 MiB of stack where Linux and macOS give
    // 8 MiB, which the CLI parser and the async command futures can exceed.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608");
        }
    }

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("version.txt");
    fs::write(output, version.trim_end()).expect("write version metadata");
}
