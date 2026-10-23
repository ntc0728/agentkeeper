// The Tauri build script.
//
// `tauri_build::build()` copies the sidecars declared in
// `bundle.externalBin` next to the compiled binary, and it HARD FAILS when one
// of them is missing:
//
//   resource path `binaries\agentkeeper-mcp-x86_64-pc-windows-msvc.exe` doesn't exist
//
// The two sidecars are the MCP server and the headless scanner. They live in
// the `agentkeeper-core` crate — a different crate, deliberately, so they do not
// inherit the WebView dependency graph — and `tauri-shell/stage-core-bins.js`
// builds and stages them.
//
// That makes this a SEQUENCE requirement, not a circular one:
//
//   cargo test / cargo build (shell)   →  needs the sidecars staged
//   stage-core-bins.js                 →  builds the core crate ONLY
//
// The core crate has no build script, so staging it never needs the shell
// compiled. Run `npm run tauri:core-bins` once after a clone, and the shell
// compiles from then on.
//
// The check below turns tauri's terse error into an actionable one. It does
// not work around anything: if a sidecar is genuinely missing, the build must
// still fail.

use std::path::Path;

/// What the user has to run, and why.
const HINT: &str = "\
error: the MCP server / headless scanner sidecars are not staged.

They are built from the `agentkeeper-core` crate by:

    npm run tauri:core-bins

(that script builds only the core crate — it does not need the shell compiled,
so there is no circular dependency). `npm run tauri:bundle` does this for you.

To see the original failure, run cargo again with the sidecars present.";

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let binaries = Path::new(&manifest_dir).join("binaries");

    // Only the sidecars this crate's config actually declares are checked, so
    // the message never points at a file that is not wanted.
    let declared = ["agentkeeper-mcp", "ak-scan"];
    let missing: Vec<String> = declared
        .iter()
        .map(|name| {
            let file = binaries.join(format!("{name}-x86_64-pc-windows-msvc.exe"));
            if file.exists() {
                None
            } else {
                Some(format!("  missing: {}", file.display()))
            }
        })
        .flatten()
        .collect();

    if !missing.is_empty() {
        for line in &missing {
            println!("cargo:warning={line}");
        }
        println!("{HINT}");
        // Fail with a message a human can act on rather than tauri's
        // `resource path ... doesn't exist` further down the log.
        std::process::exit(1);
    }

    tauri_build::build()
}
