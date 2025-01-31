use std::path::Path;

use miette::{bail, Context as _, IntoDiagnostic as _};

use crate::{appcfg::{BuildMethod, BuildPlan}, cmd_with_clean_env};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LinkStyle {
    Partial,
    Full,
}

#[allow(clippy::too_many_arguments)]
pub fn do_cargo_build(
    linker_script_text: &str,
    plan: &BuildPlan,
    link_style: LinkStyle,
    targetroot: &Path,
    tmpdir: &Path,
    outdir: &Path,
    product_name: &str,
    cargo_verbose: bool,
    build_verbose: bool,
) -> miette::Result<()> {
    let comma_features = itertools::join(&plan.cargo_features, ",");
    if build_verbose {
        let mut rows = vec![
            ("Product", product_name),
            ("Package", &plan.package_name),
            ("Binary", &plan.bin_name),
            ("Target", &plan.target_triple),
            ("Default Features", if plan.default_features { "true" } else { "false" }),
            ("Features", &comma_features),
        ];
        if let Some(tc) = &plan.toolchain_override {
            rows.push(("Toolchain Override", tc));
        }
        let joined_rustflags = plan.rustflags.join("\n");
        rows.push(("RUSTFLAGS", &joined_rustflags));
        for (k, v) in &plan.smuggled_env {
            rows.push((k, v));
        }
        crate::verbose::simple_table(rows);
    } else {
        crate::verbose::banner(format!("building component: {product_name}"));
    }
    
    let mut cmd = cmd_with_clean_env("cargo");

    if let Some(tc) = &plan.toolchain_override {
        cmd.arg(format!("+{tc}"));
    }

    let linker_script_copy = tmpdir.join("link.x");
    std::fs::write(&linker_script_copy, linker_script_text).into_diagnostic()?;

    let mut rustflags = vec![
        format!("-Clink-arg=-L{}", tmpdir.display()),
        format!("-Clink-arg=-T{}", linker_script_copy.display()),
    ];
    match link_style {
        LinkStyle::Partial => rustflags.push("-Clink-arg=-r".to_string()),
        LinkStyle::Full => (),
    }
    rustflags.extend(plan.rustflags.iter().cloned());

    cmd.env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\u{001f}"));

    let product_path = match &plan.method {
        BuildMethod::CargoWorkspaceBuild => {
            cmd.args(["build", "--release"]);
            cmd.args(["-p", &plan.package_name, "--bin", &plan.bin_name]);

            let mut outloc = targetroot.join(&plan.target_triple);
            outloc.push("release");
            outloc.push(&plan.bin_name);
            outloc
        }
        BuildMethod::CargoInstallGit { repo, rev } => {
            cmd.args(["install", "--locked", "--no-track", "--force"]);
            cmd.arg(&plan.package_name);
            cmd.args(["--bin", &plan.bin_name]);
            cmd.args(["--git", repo]);
            cmd.args(["--rev", rev]);

            let installroot = tmpdir.join(format!("{product_name}.cargo-install"));
            let targetdir = tmpdir.join(format!("{product_name}.cargo-target"));

            cmd.arg("--root");
            cmd.arg(&installroot);

            let bindir = installroot.join("bin");
            let binpath = bindir.join(&plan.bin_name);

            // Extend the PATH to add our output directory so that cargo won't
            // unconditionally warn about every build, sigh.
            match std::env::var_os("PATH") {
                Some(path) => {
                    let mut parts = std::env::split_paths(&path).collect::<Vec<_>>();
                    parts.push(bindir.clone());
                    let new_path = std::env::join_paths(parts).into_diagnostic()?;
                    cmd.env("PATH", new_path);
                }
                None => {
                    cmd.env("PATH", &bindir);
                }
            }

            cmd.env("CARGO_TARGET_DIR", targetdir);

            binpath
        }
    };

    if cargo_verbose {
        cmd.arg("--verbose");
    }

    cmd.args(["--target", &plan.target_triple]);

    if !plan.default_features {
        cmd.arg("--no-default-features");
    }
    if !plan.cargo_features.is_empty() {
        cmd.args(["--features", &comma_features]);
    }

    for (k, v) in &plan.smuggled_env {
        cmd.env(k, v);
    }

    let status = cmd.status().into_diagnostic()?;
    if !status.success() {
        bail!("failed to build, see output");
    }

    std::fs::remove_file(linker_script_copy).into_diagnostic()?;

    let final_path = outdir.join(product_name);
    std::fs::copy(&product_path, &final_path).into_diagnostic()
        .with_context(|| format!("copying {} to {}", product_path.display(), final_path.display()))?;

    Ok(())
}


