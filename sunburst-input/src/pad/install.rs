// SPDX-License-Identifier: GPL-2.0-or-later

//! Stage the HIDMaestro UMDF2 driver into the Windows **driver store**.
//!
//! This is the once-per-machine half of bringing a virtual pad up. It is the
//! driver-*store* step only — it makes `HIDMaestro.dll` available for PnP to
//! bind. The per-controller device node, and the bind against it, live in
//! [`super::device`]; the two are deliberately separate because the store
//! install is idempotent and machine-wide while a device node is per-pad and
//! per-session.
//!
//! **Mechanism.** `pnputil /add-driver <inf> /install`, the documented modern
//! path and the one HIDMaestro's own `DriverBuilder.FullDeploy` uses. It adds
//! the package to the driver store (persisting across reboots — repeat calls
//! are a no-op) and installs it against any already-present matching device.
//! `SetupCopyOEMInfW` is the in-process alternative; `pnputil` is preferred here
//! because it validates the catalog and reports a clear published name, and this
//! is a control-plane action off the video path where a subprocess is free.
//!
//! **Signing.** UMDF2 driver packages are catalog-signed. The 4070 box must
//! either trust the release's signing certificate (import the `.cer` into
//! `LocalMachine\TrustedPublisher` + `Root`) or run with test-signing enabled
//! (`bcdedit /set testsigning on`, then reboot). `pnputil /add-driver` fails
//! with `ERROR_DRIVER_STORE_ADD_FAILED` / a signature error otherwise; that is
//! surfaced verbatim so the cause is unambiguous. Signing is a box-provisioning
//! step, not something this code performs.
//!
//! Windows-only: gated at the module in `pad/mod.rs`, so it never compiles on
//! the Linux development host.

use std::path::Path;
use std::process::Command;

/// A driver package that has been added to the store.
///
/// Held so the caller can name it in diagnostics and, if it ever wants to, undo
/// the install with [`uninstall`]. Dropping it does **not** remove the package —
/// the driver store is intentionally durable, and a pad's lifetime is its device
/// node ([`super::device`]), not the store entry.
#[derive(Debug, Clone)]
pub struct InstalledDriver {
    /// The `oemNN.inf` name `pnputil` published the package under, when it could
    /// be parsed from the output. Informational; the store is keyed internally.
    pub published_inf: Option<String>,
}

/// `pnputil /add-driver <inf> /install`. Idempotent: adding a package already in
/// the store re-reports the existing published name and returns success.
///
/// `inf_path` must point at the vendored `hidmaestro.inf` (its `HIDMaestro.dll`
/// must sit beside it, as the extracted release layout has them).
pub fn ensure_installed(inf_path: &Path) -> Result<InstalledDriver, String> {
    if !inf_path.exists() {
        return Err(format!("driver INF not found: {}", inf_path.display()));
    }

    let output = Command::new("pnputil.exe")
        .arg("/add-driver")
        .arg(inf_path)
        .arg("/install")
        .output()
        .map_err(|e| format!("failed to launch pnputil: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // pnputil's exit code is 0 on success and 3010 (ERROR_SUCCESS_REBOOT_REQUIRED)
    // when a reboot is pending; both mean the package is in the store.
    let code = output.status.code().unwrap_or(-1);
    if code != 0 && code != 3010 {
        return Err(format!(
            "pnputil /add-driver failed (exit {code}): {}",
            first_meaningful_line(&stdout, &stderr)
        ));
    }

    Ok(InstalledDriver {
        published_inf: parse_published_inf(&stdout),
    })
}

/// `pnputil /delete-driver <oemNN.inf> /uninstall /force`. Removes the package
/// from the store. Only needed to fully retract a driver from the box — not part
/// of normal pad teardown.
pub fn uninstall(published_inf: &str) -> Result<(), String> {
    let output = Command::new("pnputil.exe")
        .arg("/delete-driver")
        .arg(published_inf)
        .arg("/uninstall")
        .arg("/force")
        .output()
        .map_err(|e| format!("failed to launch pnputil: {e}"))?;

    let code = output.status.code().unwrap_or(-1);
    if code != 0 && code != 3010 {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "pnputil /delete-driver failed (exit {code}): {}",
            first_meaningful_line(&stdout, &stderr)
        ));
    }
    Ok(())
}

/// Pull the published `oemNN.inf` name out of pnputil's success output. Best
/// effort — a missing name is not an error, since the install still succeeded.
fn parse_published_inf(stdout: &str) -> Option<String> {
    // pnputil prints a line like "Published Name:  oem42.inf" (localised label,
    // stable token). Match the token rather than the label.
    stdout
        .split_whitespace()
        .find(|tok| {
            let t = tok.trim_end_matches(&['\r', '\n'][..]);
            t.len() > 4
                && t.to_ascii_lowercase().starts_with("oem")
                && t.to_ascii_lowercase().ends_with(".inf")
        })
        .map(|t| t.trim_end_matches(&['\r', '\n'][..]).to_string())
}

/// The first non-empty line across stderr then stdout, for a compact error.
fn first_meaningful_line(stdout: &str, stderr: &str) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_published_name_from_pnputil_output() {
        let out = "Microsoft PnP Utility\r\n\r\nAdding driver package:  hidmaestro.inf\r\nDriver package added successfully.\r\nPublished Name:         oem73.inf\r\n";
        assert_eq!(parse_published_inf(out), Some("oem73.inf".to_string()));
    }

    #[test]
    fn absent_published_name_is_not_an_error() {
        assert_eq!(
            parse_published_inf("Driver package added successfully.\r\n"),
            None
        );
    }

    #[test]
    fn error_line_prefers_stderr_then_falls_back() {
        assert_eq!(first_meaningful_line("", "  \nboom\n"), "boom");
        assert_eq!(first_meaningful_line("out only\n", "   \n"), "out only");
        assert_eq!(first_meaningful_line("", ""), "(no output)");
    }
}
