// SPDX-License-Identifier: MIT OR Apache-2.0

//! avb-installer: host app for the AVB driver system extensions.
//!
//! This binary lives at `AVBInstaller.app/Contents/MacOS/avb-installer` and is
//! executed directly, so `NSBundle.mainBundle` is the app bundle and sysextd
//! finds the dext in `Contents/Library/SystemExtensions/`.
//!
//! Written in Rust with objc2 + objc2-system-extensions (plan decision 7). No
//! Swift/ObjC host is needed: the bindings cover OSSystemExtensionRequest and
//! its delegate protocol.
//!
//! Subcommands:
//!   activate                          activate the I210 dext and wait for the result
//!   deactivate [--dext i210|bcm57762] deactivate a dext (default i210) and wait
//!   status                            print `systemextensionsctl list` lines for both dexts

use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread};
use objc2_foundation::{
    NSBundle, NSDate, NSError, NSObject, NSObjectProtocol, NSRunLoop, NSString,
};
use objc2_system_extensions::{
    OSSystemExtensionErrorCode, OSSystemExtensionManager, OSSystemExtensionProperties,
    OSSystemExtensionReplacementAction, OSSystemExtensionRequest, OSSystemExtensionRequestDelegate,
    OSSystemExtensionRequestResult,
};

const APP_BUNDLE_ID: &str = "com.avbaudio.installer";
const DEXT_I210: &str = "com.avbaudio.installer.i210";
const DEXT_BCM: &str = "com.avbaudio.installer.bcm57762";
const RUN_LOOP_TIMEOUT_S: f64 = 300.0;

/// Readable name for an `OSSystemExtensionErrorDomain` code.
fn error_code_name(code: isize) -> &'static str {
    match OSSystemExtensionErrorCode(code) {
        OSSystemExtensionErrorCode::Unknown => "Unknown",
        OSSystemExtensionErrorCode::MissingEntitlement => "MissingEntitlement",
        OSSystemExtensionErrorCode::UnsupportedParentBundleLocation => {
            "UnsupportedParentBundleLocation (app must be in /Applications)"
        }
        OSSystemExtensionErrorCode::ExtensionNotFound => "ExtensionNotFound",
        OSSystemExtensionErrorCode::ExtensionMissingIdentifier => "ExtensionMissingIdentifier",
        OSSystemExtensionErrorCode::DuplicateExtensionIdentifer => "DuplicateExtensionIdentifier",
        OSSystemExtensionErrorCode::UnknownExtensionCategory => "UnknownExtensionCategory",
        OSSystemExtensionErrorCode::CodeSignatureInvalid => "CodeSignatureInvalid",
        OSSystemExtensionErrorCode::ValidationFailed => "ValidationFailed",
        OSSystemExtensionErrorCode::ForbiddenBySystemPolicy => "ForbiddenBySystemPolicy",
        OSSystemExtensionErrorCode::RequestCanceled => "RequestCanceled",
        OSSystemExtensionErrorCode::RequestSuperseded => "RequestSuperseded",
        OSSystemExtensionErrorCode::AuthorizationRequired => "AuthorizationRequired",
        _ => "unrecognised",
    }
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - AvbRequestDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "AvbRequestDelegate"]
    struct AvbRequestDelegate;

    unsafe impl NSObjectProtocol for AvbRequestDelegate {}

    unsafe impl OSSystemExtensionRequestDelegate for AvbRequestDelegate {
        #[unsafe(method(request:actionForReplacingExtension:withExtension:))]
        fn action_for_replacing(
            &self,
            _request: &OSSystemExtensionRequest,
            existing: &OSSystemExtensionProperties,
            ext: &OSSystemExtensionProperties,
        ) -> OSSystemExtensionReplacementAction {
            let (old, new) = unsafe { (existing.bundleVersion(), ext.bundleVersion()) };
            println!("replacing installed extension version {old} with {new}");
            OSSystemExtensionReplacementAction::Replace
        }

        #[unsafe(method(requestNeedsUserApproval:))]
        fn needs_user_approval(&self, _request: &OSSystemExtensionRequest) {
            println!(
                "user approval required: open System Settings > General > Login Items & \
                 Extensions > Driver Extensions and enable AVBI210Driver. Waiting..."
            );
        }

        #[unsafe(method(request:didFinishWithResult:))]
        fn did_finish(
            &self,
            _request: &OSSystemExtensionRequest,
            result: OSSystemExtensionRequestResult,
        ) {
            match result {
                OSSystemExtensionRequestResult::Completed => println!("request completed"),
                OSSystemExtensionRequestResult::WillCompleteAfterReboot => {
                    println!("request will complete after reboot")
                }
                other => println!("request finished with result {}", other.0),
            }
            std::process::exit(0);
        }

        #[unsafe(method(request:didFailWithError:))]
        fn did_fail(&self, request: &OSSystemExtensionRequest, error: &NSError) {
            let code = error.code();
            let name = error_code_name(code);
            // A failure callback is NOT proof the operation failed.
            //
            // On an UPGRADE, sysextd replaces the running dext and then reports
            // ExtensionNotFound for the one it just superseded — so this fires
            // with code 4 on a request that actually succeeded. Measured:
            // activating 284.2.33 over a live 283.0.31 printed
            // "request failed: ... code 4 = ExtensionNotFound" while
            // `systemextensionsctl list` showed 284.2.33 [activated enabled]
            // and the old one "terminating for upgrade via delegate".
            //
            // Exiting non-zero on that wastes a hardware session chasing a
            // driver that is already loaded, so check the real state before
            // believing the error.
            let ident = unsafe { request.identifier() }.to_string();
            match activated_enabled_version(&ident) {
                Some(v) => {
                    println!(
                        "request reported an error ({name}, code {code}) but \
                         {ident} is now activated and enabled at version {v} — \
                         treating it as a successful upgrade"
                    );
                    eprintln!("  the error was: {}", error.localizedDescription());
                    std::process::exit(0);
                }
                None => {
                    eprintln!(
                        "request failed: {} (domain {}, code {} = {name})",
                        error.localizedDescription(),
                        error.domain(),
                        code,
                    );
                    std::process::exit(2);
                }
            }
        }
    }
);

/// The version of `ident` that `systemextensionsctl` reports as activated AND
/// enabled, if any.
///
/// Shelling out rather than asking the framework: there is no public API for
/// "what is active right now", and this is the same list a human would read to
/// settle the question. Lines look like:
///
/// ```text
/// *  *  -  com.avbaudio.installer.i210 (284.2.33/284.2.33)  AVBI210Driver  [activated enabled]
/// ```
///
/// Both leading `*` columns (enabled, active) must be set, and the state must
/// be `activated enabled` — a dext that is merely staged, disabled, or
/// terminating does not count as success.
fn activated_enabled_version(ident: &str) -> Option<String> {
    let out = Command::new("/usr/bin/systemextensionsctl")
        .arg("list")
        .output()
        .ok()?;
    parse_activated_enabled(&String::from_utf8_lossy(&out.stdout), ident)
}

/// The pure parser behind [`activated_enabled_version`], split out so it can be
/// tested against captured `systemextensionsctl` output instead of whatever
/// this machine happens to have installed.
fn parse_activated_enabled(text: &str, ident: &str) -> Option<String> {
    text.lines()
        .filter(|l| l.contains(ident) && l.contains("[activated enabled]"))
        .find_map(|l| {
            // The two status columns precede the team id and bundle id.
            let cols: Vec<&str> = l.split('\t').collect();
            let enabled = cols.first().is_some_and(|c| c.trim() == "*");
            let active = cols.get(1).is_some_and(|c| c.trim() == "*");
            if !(enabled && active) {
                return None;
            }
            // "(284.2.33/284.2.33)" -> "284.2.33"
            let open = l.find('(')?;
            let close = l.get(open..)?.find(')')? + open;
            let span = l.get(open + 1..close)?;
            Some(span.split('/').next().unwrap_or(span).to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `systemextensionsctl list` output captured during the upgrade that
    /// exposed the misreport: 284.2.33 went live while sysextd reported
    /// ExtensionNotFound, and 283.0.31 was still terminating.
    const LIST: &str = "\
--- com.apple.system_extension.driver_extension (Go to 'System Settings' to modify)
enabled\tactive\tteamID\tbundleID (version)\tname\t[state]
*\t*\t-\tcom.avbaudio.installer.i210 (284.2.33/284.2.33)\tAVBI210Driver\t[activated enabled]
*\t*\t64H3QYW26T\tcom.fender.tusbaudiodriver (1.15.0/1.15.0)\tcom.fender.tusbaudiodriver\t[activated enabled]
\t*\t8NG8NMGYVF\tcom.avbaudio.installer.i210 (282.7.31/282.7.31)\tAVBI210Driver\t[activated disabled]
\t\t-\tcom.avbaudio.installer.i210 (283.0.31/283.0.31)\tAVBI210Driver\t[terminating for upgrade via delegate]
";

    #[test]
    fn finds_the_activated_enabled_version_among_several_entries() {
        assert_eq!(
            parse_activated_enabled(LIST, DEXT_I210),
            Some("284.2.33".to_string())
        );
    }

    /// The three non-live states in that same output must NOT be read as
    /// success: a disabled one, a terminating one, and another vendor's dext.
    #[test]
    fn staged_disabled_and_terminating_entries_do_not_count() {
        // Strip the one live line; nothing else may satisfy the query, even
        // though two other i210 entries remain.
        let without_live: String = LIST
            .lines()
            .filter(|l| !l.contains("284.2.33"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            without_live.contains("282.7.31"),
            "fixture still has others"
        );
        assert!(without_live.contains("283.0.31"));
        assert_eq!(parse_activated_enabled(&without_live, DEXT_I210), None);
    }

    #[test]
    fn another_bundle_id_is_not_ours() {
        assert_eq!(
            parse_activated_enabled(LIST, "com.fender.tusbaudiodriver"),
            Some("1.15.0".to_string())
        );
        assert_eq!(parse_activated_enabled(LIST, DEXT_BCM), None);
    }

    /// An entry marked `[activated enabled]` but missing a status column is
    /// not live, so the state string alone must not be trusted.
    #[test]
    fn the_state_string_alone_is_not_enough() {
        let half =
            "\t*\t-\tcom.avbaudio.installer.i210 (9.9.9/9.9.9)\tAVBI210Driver\t[activated enabled]";
        assert_eq!(parse_activated_enabled(half, DEXT_I210), None);
    }

    #[test]
    fn empty_or_malformed_output_is_not_a_version() {
        assert_eq!(parse_activated_enabled("", DEXT_I210), None);
        assert_eq!(parse_activated_enabled("garbage", DEXT_I210), None);
        // Marked live but with no version parenthesis to read.
        assert_eq!(
            parse_activated_enabled(
                "*\t*\t-\tcom.avbaudio.installer.i210\tAVBI210Driver\t[activated enabled]",
                DEXT_I210
            ),
            None
        );
    }
}

impl AvbRequestDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Activate,
    Deactivate,
}

fn check_bundle() -> Result<()> {
    let bundle = NSBundle::mainBundle();
    let id = bundle.bundleIdentifier().map(|s| s.to_string());
    if id.as_deref() != Some(APP_BUNDLE_ID) {
        bail!(
            "must run from inside AVBInstaller.app (main bundle identifier is {:?}, expected {APP_BUNDLE_ID}; \
             bundle path {})",
            id,
            bundle.bundlePath()
        );
    }
    Ok(())
}

fn submit(action: Action, dext: &str) -> Result<()> {
    check_bundle()?;
    let ident = NSString::from_str(dext);
    let queue = DispatchQueue::main();
    let request = unsafe {
        match action {
            Action::Activate => {
                OSSystemExtensionRequest::activationRequestForExtension_queue(&ident, queue)
            }
            Action::Deactivate => {
                OSSystemExtensionRequest::deactivationRequestForExtension_queue(&ident, queue)
            }
        }
    };
    // The delegate property is weak: keep `delegate` alive for the whole run loop.
    let delegate = AvbRequestDelegate::new();
    unsafe {
        request.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        OSSystemExtensionManager::sharedManager().submitRequest(&request);
    }
    println!(
        "{} request submitted for {dext}; waiting up to {RUN_LOOP_TIMEOUT_S:.0} s",
        match action {
            Action::Activate => "activation",
            Action::Deactivate => "deactivation",
        }
    );
    // Delegate callbacks arrive on the main queue, which the main run loop drains.
    // They exit the process; returning from here means we timed out.
    NSRunLoop::mainRunLoop()
        .runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(RUN_LOOP_TIMEOUT_S));
    drop(delegate);
    bail!("timed out after {RUN_LOOP_TIMEOUT_S:.0} s waiting for sysextd")
}

fn status() -> Result<()> {
    let out = Command::new("/usr/bin/systemextensionsctl")
        .arg("list")
        .output()
        .context("running systemextensionsctl list")?;
    let text = String::from_utf8_lossy(&out.stdout);
    for id in [DEXT_I210, DEXT_BCM] {
        let mut found = false;
        for line in text.lines().filter(|l| l.contains(id)) {
            println!("{line}");
            found = true;
        }
        if !found {
            println!("{id}: not installed");
        }
    }
    Ok(())
}

fn usage() {
    eprintln!("usage: avb-installer <activate | deactivate [--dext i210|bcm57762] | status>");
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match argv.as_slice() {
        ["activate"] => submit(Action::Activate, DEXT_I210),
        ["deactivate"] | ["deactivate", "--dext", "i210"] => submit(Action::Deactivate, DEXT_I210),
        ["deactivate", "--dext", "bcm57762"] => submit(Action::Deactivate, DEXT_BCM),
        ["status"] => status(),
        _ => {
            usage();
            return ExitCode::from(64);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
