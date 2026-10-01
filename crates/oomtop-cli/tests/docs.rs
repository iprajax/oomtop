//! Keeps user docs and packaging in sync with the code (CLAUDE.md: "new setting → config docs + schema").

use clap::CommandFactory;
use std::path::PathBuf;
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn every_setting_is_in_docs_config_md() {
    let doc = read("docs/config.md");
    let missing: Vec<&str> = oomtop_config::docs::DOCS
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !doc.contains(&format!("`{k}`")))
        .collect();
    assert!(missing.is_empty(), "docs/config.md lacks: {missing:?}");
}

#[test]
fn every_subcommand_is_in_docs_cli_md() {
    let doc = read("docs/cli.md");
    let cmd = oomtop_cli::Cli::command();
    let mut missing = Vec::new();
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        if !doc.contains(&format!("oomtop {name}")) {
            missing.push(name.to_string());
        }
        for s in sub.get_subcommands() {
            if !doc.contains(s.get_name()) {
                missing.push(format!("{name} {}", s.get_name()));
            }
        }
    }
    for arg in cmd.get_arguments() {
        if let Some(l) = arg.get_long() {
            if !arg.is_hide_set() && l != "help" && l != "version" && !doc.contains(&format!("--{l}")) {
                missing.push(format!("--{l}"));
            }
        }
    }
    assert!(missing.is_empty(), "docs/cli.md lacks: {missing:?}");
}

#[test]
fn readme_and_docs_link_targets_exist() {
    for f in [
        "README.md",
        "LICENSE",
        "docs/config.md",
        "docs/themes.md",
        "docs/mcp.md",
        "docs/cli.md",
        "tools/groundtruth/README.md",
        "tools/groundtruth/groundtruth.py",
        "tools/groundtruth/perf_budget.py",
        "packaging/install.sh",
        "packaging/homebrew/oomtop.rb.tmpl",
        "packaging/homebrew/render.py",
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
    ] {
        assert!(root().join(f).is_file(), "{f} missing");
    }
    let readme = read("README.md");
    assert!(readme.contains("claude mcp add oomtop -- oomtop mcp"));
    assert!(readme.contains("MIT"));
    assert!(read("LICENSE").starts_with("MIT License"));
}

#[test]
fn deb_and_rpm_assets_exist() {
    let manifest: toml::Table = read("crates/oomtop-cli/Cargo.toml").parse().unwrap();
    let meta = &manifest["package"]["metadata"];
    let pkg_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut sources: Vec<String> = meta["deb"]["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a[0].as_str().unwrap().to_string())
        .collect();
    sources.extend(
        meta["generate-rpm"]["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["source"].as_str().unwrap().to_string()),
    );
    for s in sources {
        if s.starts_with("target/") {
            continue; // the built binary
        }
        assert!(pkg_dir.join(&s).is_file(), "packaging asset {s} missing");
    }
    assert_eq!(meta["generate-rpm"]["license"].as_str(), Some("MIT"));
}

#[cfg(unix)]
#[test]
fn install_script_is_valid_posix_sh() {
    let out = Command::new("sh")
        .arg("-n")
        .arg(root().join("packaging/install.sh"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = read("packaging/install.sh");
    assert!(text.contains("checksum mismatch"), "verifies checksums");
    assert!(!text.contains("sudo "), "never sudo");
}

#[cfg(unix)]
#[test]
fn install_script_verifies_the_checksum_end_to_end() {
    // A fake release on disk: good archive installs, tampered archive is refused.
    let d = tempfile::tempdir().unwrap();
    let (os, target) = if cfg!(target_os = "macos") {
        ("mac", "universal-apple-darwin".to_string())
    } else {
        // The script installs for the machine `uname -m` reports — not the test binary's target (an x86_64
        // test run under Rosetta/QEMU on an arm64 host still installs the aarch64 build).
        let m = Command::new("uname").arg("-m").output().unwrap();
        let arch = match String::from_utf8_lossy(&m.stdout).trim() {
            "aarch64" | "arm64" => "aarch64",
            _ => "x86_64",
        };
        ("linux", format!("{arch}-unknown-linux-musl"))
    };
    let name = format!("oomtop-9.9.9-{target}");
    let stage = d.path().join("stage").join(&name);
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("oomtop"), "#!/bin/sh\necho oomtop 9.9.9\n").unwrap();
    let rel = d.path().join("rel/v9.9.9");
    std::fs::create_dir_all(&rel).unwrap();
    let tgz = rel.join(format!("{name}.tar.gz"));
    let ok = Command::new("tar")
        .arg("-C")
        .arg(d.path().join("stage"))
        .arg("-czf")
        .arg(&tgz)
        .arg(&name)
        .status()
        .unwrap();
    assert!(ok.success());
    let sum = {
        use sha2::Digest;
        let bytes = std::fs::read(&tgz).unwrap();
        let h = sha2::Sha256::digest(&bytes);
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    std::fs::write(rel.join("SHA256SUMS"), format!("{sum}  {name}.tar.gz\n")).unwrap();
    let run = |to: &str| {
        let mut c = Command::new("sh");
        c.arg(root().join("packaging/install.sh"))
            .args(["--version", "9.9.9", "--to"])
            .arg(d.path().join(to))
            .env(
                "OOMTOP_BASE_URL",
                format!("file://{}", d.path().join("rel").display()),
            );
        if os == "linux" {
            c.arg("--musl");
        }
        c.output().unwrap()
    };
    let out = run("bin");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(d.path().join("bin/oomtop").is_file());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&tgz)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"tampered"))
        .unwrap();
    let out = run("bin2");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
    assert!(
        !d.path().join("bin2/oomtop").exists(),
        "nothing installed on mismatch"
    );
    // A mirror over plain HTTP is refused before anything is downloaded.
    let out = Command::new("sh")
        .arg(root().join("packaging/install.sh"))
        .args(["--version", "9.9.9", "--to"])
        .arg(d.path().join("bin3"))
        .env("OOMTOP_BASE_URL", "http://127.0.0.1:9/rel")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("non-HTTPS"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!d.path().join("bin3").exists());
}

#[test]
fn deb_depends_pin_the_glibc_baseline() {
    // `$auto` would run dpkg-shlibdeps on the runner; the floor must be the zigbuild's glibc 2.28 baseline.
    let manifest: toml::Table = read("crates/oomtop-cli/Cargo.toml").parse().unwrap();
    let deb = &manifest["package"]["metadata"]["deb"];
    let depends = deb["depends"].as_str().unwrap();
    assert!(!depends.contains("$auto"), "{depends}");
    assert!(depends.contains("libc6 (>= 2.28)"), "glibc baseline: {depends}");
}

#[test]
fn workflows_cover_the_spec_matrix_and_release_artifacts() {
    let ci = read(".github/workflows/ci.yml");
    for os in ["ubuntu-latest", "ubuntu-24.04-arm", "macos-latest"] {
        assert!(ci.contains(os), "ci matrix lacks {os}");
    }
    assert!(ci.contains("cargo fmt --all -- --check"));
    assert!(ci.contains("-- -D warnings"));
    assert!(ci.contains("cron:"), "nightly perf");
    assert!(ci.contains("perf_budget.py"));
    assert!(ci.contains("groundtruth.py --selftest"));
    let rel = read(".github/workflows/release.yml");
    for needle in [
        "x86_64-unknown-linux-gnu.2.28",
        "aarch64-unknown-linux-gnu.2.28",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "cargo zigbuild",
        "lipo -create",
        "codesign",
        "notarytool",
        "HAS_CERT == 'true'",
        "SHA256SUMS",
        "cargo deb",
        "cargo generate-rpm",
        "render.py",
    ] {
        assert!(rel.contains(needle), "release.yml lacks {needle}");
    }
    // Manual dry runs build the requested tag, not the default branch.
    assert!(rel.contains("ref: ${{ github.event.inputs.tag || github.ref }}"));
}

#[cfg(unix)]
#[test]
fn groundtruth_parsers_selftest() {
    // stdlib-only Python; skipped where no python3 exists (CI runners and this Mac have it).
    let Ok(out) = Command::new("python3")
        .arg(root().join("tools/groundtruth/groundtruth.py"))
        .arg("--selftest")
        .output()
    else {
        eprintln!("python3 not found; skipping");
        return;
    };
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("selftest: ok"));
}
