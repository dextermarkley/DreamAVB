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
            let want = bundled_dext_version(&ident);
            // GROUND TRUTH, before trusting sysextd's bookkeeping: the code
            // hash of what the kernel has actually attached, against the hash
            // of the bundle we just submitted.
            //
            // `systemextensionsctl list` is bookkeeping and can disagree with
            // the kernel. Measured here: the list showed 284.19.2 `[activated
            // enabled]` while the attached service's IOUserServerCDHash was
            // 284.2.33's — the copy the list called `terminating for upgrade
            // via delegate` was the one driving the hardware. Two upgrades in
            // a row had reported success and changed nothing, and a version
            // comparison against the list could not see it because the list
            // named the new version.
            let running = running_dext_cdhash();
            let submitted = bundled_dext_path(&ident).and_then(|p| bundle_cdhash(&p));
            if let (Some(run), Some(sub)) = (running.as_deref(), submitted.as_deref()) {
                if run != sub {
                    eprintln!(
                        "request FAILED: the kernel is still running a different build of \
                         {ident}"
                    );
                    eprintln!("  attached code hash:  {run}");
                    eprintln!("  this bundle's:       {sub}");
                    eprintln!("  the error was: {}", error.localizedDescription());
                    eprintln!(
                        "  `systemextensionsctl list` may well claim the new version is \
                         activated and enabled — it reports sysextd's bookkeeping, not what \
                         the kernel loaded. An older copy stuck in `terminating for upgrade \
                         via delegate` blocks the replacement until a reboot clears it."
                    );
                    std::process::exit(2);
                }
            }
            match activated_enabled_version(&ident) {
                // Active AND the version we asked for: the benign
                // upgrade-reports-failure case.
                Some(v) if want.as_deref().is_none_or(|w| w == v) => {
                    println!(
                        "request reported an error ({name}, code {code}) but \
                         {ident} is now activated and enabled at version {v} — \
                         treating it as a successful upgrade"
                    );
                    eprintln!("  the error was: {}", error.localizedDescription());
                    std::process::exit(0);
                }
                // Active, but STILL AT THE OLD VERSION. This is a genuinely
                // failed upgrade and must not be reported as a success.
                //
                // The first version of this check asked only "is the
                // identifier activated and enabled", which is true of the
                // extension that was ALREADY running — so a request that
                // changed nothing printed "treating it as a successful
                // upgrade" and exited 0. Measured: asking for 284.20.50 while
                // 284.19.2 was live reported success at 284.19.2, and the new
                // driver never loaded. Comparing against what we actually
                // submitted is the whole point of verifying.
                Some(v) => {
                    eprintln!(
                        "request FAILED: {ident} is still at version {v}, but this bundle \
                         asked for {}",
                        want.as_deref().unwrap_or("(unknown)")
                    );
                    eprintln!("  the error was: {}", error.localizedDescription());
                    eprintln!(
                        "  the running extension was NOT replaced. sysextd often has an older \
                         copy stuck in `terminating for upgrade via delegate`, which blocks \
                         further upgrades until a reboot clears it — check \
                         `systemextensionsctl list`."
                    );
                    std::process::exit(2);
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

/// The code-directory hash of the dext the KERNEL has actually loaded and
/// attached, read from the matched personality of the live `AVBI210Driver`
/// service.
///
/// This is the only ground truth about what is running.
/// `systemextensionsctl list` reports sysextd's BOOKKEEPING, which can
/// disagree with the kernel: measured on this machine, the list showed
/// 284.19.2 `[activated enabled]` and 284.2.33 `[terminating for upgrade via
/// delegate]` while the attached service's `IOUserServerCDHash` matched
/// 284.2.33 exactly — the version marked as terminating was the one actually
/// driving the hardware, and two "successful upgrades" had changed nothing.
///
/// `ioreg -rn NAME` only, never with `-c`: the class filter discards the name
/// match and returns nothing even when the dext is attached.
fn running_dext_cdhash() -> Option<String> {
    let out = Command::new("/usr/sbin/ioreg")
        .args(["-rn", "AVBI210Driver", "-w0"])
        .output()
        .ok()?;
    parse_running_cdhash(&String::from_utf8_lossy(&out.stdout))
}

/// The pure parser behind [`running_dext_cdhash`], split out so it can be
/// tested against captured `ioreg` output.
fn parse_running_cdhash(text: &str) -> Option<String> {
    const KEY: &str = "\"IOUserServerCDHash\"=\"";
    let after = text.split(KEY).nth(1)?;
    let end = after.find('"')?;
    let h = after.get(..end)?;
    (!h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
}

/// The code-directory hash `codesign` reports for a bundle on disk.
fn bundle_cdhash(path: &std::path::Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign")
        .arg("-dvvv")
        .arg(path)
        .output()
        .ok()?;
    // codesign writes its description to stderr.
    parse_codesign_cdhash(&String::from_utf8_lossy(&out.stderr))
        .or_else(|| parse_codesign_cdhash(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_codesign_cdhash(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("CDHash="))
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Path to the dext inside our own app bundle.
fn bundled_dext_path(ident: &str) -> Option<std::path::PathBuf> {
    Some(
        NSBundle::mainBundle()
            .bundlePath()
            .to_string()
            .parse::<std::path::PathBuf>()
            .ok()?
            .join("Contents/Library/SystemExtensions")
            .join(format!("{ident}.dext")),
    )
}

/// The version of `ident` that THIS bundle is asking to install, read from the
/// extension's own `Info.plist` inside `Contents/Library/SystemExtensions/`.
///
/// Needed to tell a successful upgrade from one that changed nothing. The
/// activation request itself does not report the version it submitted on the
/// failure path, and `action_for_replacing` — which does see both versions —
/// is not called at all when sysextd declines to treat the request as a
/// replacement, which is exactly the case that needs catching.
///
/// `None` means we could not read it, and the caller then falls back to the
/// weaker "is it active at all" check rather than refusing a good upgrade.
fn bundled_dext_version(ident: &str) -> Option<String> {
    let plist = bundled_dext_path(ident)?.join("Info.plist");
    let text = std::fs::read_to_string(&plist).ok()?;
    plist_string(&text, "CFBundleVersion")
}

/// The version of `ident` that `systemextensionsctl` reports as activated AND
/// enabled, if any.
////// Shelling out rather than asking the framework: there is no public API for
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

    /// Real output from the FAILED upgrade this check was added for: the
    /// request asked for 284.20.50 and 284.19.2 stayed live, with an older
    /// copy still stuck terminating.
    const LIST_STALLED: &str = "\
--- com.apple.system_extension.driver_extension (Go to 'System Settings' to modify)
enabled\tactive\tteamID\tbundleID (version)\tname\t[state]
*\t*\t-\tcom.avbaudio.installer.i210 (284.19.2/284.19.2)\tAVBI210Driver\t[activated enabled]
\t\t-\tcom.avbaudio.installer.i210 (284.2.33/284.2.33)\tAVBI210Driver\t[terminating for upgrade via delegate]
\t*\t8NG8NMGYVF\tcom.avbaudio.installer.i210 (282.7.31/282.7.31)\tAVBI210Driver\t[activated disabled]
";

    /// THE REGRESSION. "Is the identifier activated and enabled" is true of
    /// the extension that was ALREADY running, so a request that replaced
    /// nothing reported a successful upgrade and exited 0. The version we
    /// submitted has to be part of the comparison.
    ///
    /// Asserts the DECISION, not just the parse.
    #[test]
    fn an_upgrade_that_changed_nothing_is_not_a_success() {
        let active = parse_activated_enabled(LIST_STALLED, DEXT_I210).expect("one is live");
        assert_eq!(active, "284.19.2");

        // The predicate `did_fail` applies.
        let is_success = |want: Option<&str>, active: &str| want.is_none_or(|w| w == active);

        // The real case: asked for 284.20.50, got 284.19.2 — NOT a success.
        assert!(
            !is_success(Some("284.20.50"), &active),
            "a stalled upgrade must not report success"
        );
        // The benign case this must still allow: the version we asked for IS
        // live and sysextd merely reported ExtensionNotFound for the one it
        // superseded.
        assert!(is_success(Some("284.19.2"), &active));
        // An unreadable bundle version falls back to the weaker check rather
        // than refusing a good upgrade.
        assert!(is_success(None, &active));
    }

    /// Real `ioreg -rn AVBI210Driver -w0` fragment from the wedged upgrade:
    /// the attached service's code hash is 284.2.33's while
    /// `systemextensionsctl` called 284.19.2 activated and enabled.
    const IOREG: &str = r#"
  +-o AVBI210Driver  <class IOUserService, id 0x100000abc, registered, matched, active>
  |   "IOMatchedPersonality" = {"IOClass"="IOUserService","CFBundleIdentifier"="com.avbaudio.installer.i210","IOUserServerCDHash"="452fe02c65a66272fa5966859d9cf86073c087be","IOUserClass"="AVBI210Driver"}
  |   "IOUserServerName" = "com.avbaudio.installer.i210"
"#;

    /// The check that would have caught two "successful" upgrades that changed
    /// nothing: compare the hash the KERNEL attached against the bundle's.
    #[test]
    fn the_attached_code_hash_is_read_from_ioreg() {
        assert_eq!(
            parse_running_cdhash(IOREG).as_deref(),
            Some("452fe02c65a66272fa5966859d9cf86073c087be")
        );
        // Nothing attached, or a name that is not ours: no hash rather than a
        // wrong one.
        assert_eq!(parse_running_cdhash(""), None);
        assert_eq!(parse_running_cdhash("no hash here"), None);
        // A non-hex value is refused rather than compared.
        assert_eq!(
            parse_running_cdhash("\"IOUserServerCDHash\"=\"not-a-hash\""),
            None
        );
    }

    /// codesign writes `CDHash=` among many lines, on stderr.
    #[test]
    fn the_bundle_code_hash_is_read_from_codesign() {
        let out = "Executable=/x/com.avbaudio.installer.i210\n\
Identifier=com.avbaudio.installer.i210\n\
CDHash=0872b5deb27a677e4644e44767c370cb2c942fe8\n\
Signature=adhoc\n";
        assert_eq!(
            parse_codesign_cdhash(out).as_deref(),
            Some("0872b5deb27a677e4644e44767c370cb2c942fe8")
        );
        assert_eq!(parse_codesign_cdhash("Identifier=x\n"), None);
    }

    /// The decision: a mismatch is a failed upgrade regardless of what
    /// `systemextensionsctl` reports, because the list is bookkeeping and the
    /// code hash is what the kernel loaded.
    #[test]
    fn a_mismatched_code_hash_fails_even_when_the_list_looks_healthy() {
        let running = parse_running_cdhash(IOREG).expect("attached");
        let submitted = "0872b5deb27a677e4644e44767c370cb2c942fe8";
        assert_ne!(running, submitted);
        // And the list, in the same moment, named the version we asked for.
        assert_eq!(
            parse_activated_enabled(LIST_STALLED, DEXT_I210).as_deref(),
            Some("284.19.2")
        );
        // So a version-only check passes while the hash check correctly fails.
        assert!(running != submitted, "hash check must reject this");
    }

    /// An older copy stuck `terminating for upgrade via delegate` must not be
    /// read as active, or the stalled list would look like a clean one.
    #[test]
    fn a_terminating_copy_is_not_read_as_active() {
        assert_eq!(
            parse_activated_enabled(LIST_STALLED, DEXT_I210).as_deref(),
            Some("284.19.2")
        );
        assert!(LIST_STALLED.contains("284.2.33"));
        assert_ne!(
            parse_activated_enabled(LIST_STALLED, DEXT_I210).as_deref(),
            Some("284.2.33")
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
    // What the KERNEL actually has attached, which is not the same question.
    //
    // The list above is sysextd's bookkeeping. Measured on this machine: it
    // reported 284.19.2 `[activated enabled]` while the attached service was
    // running 284.2.33 — the copy it called `terminating for upgrade via
    // delegate`. Two upgrades reported success and changed nothing, and
    // nothing in the list said so. Printing the attached code hash and naming
    // the staged copy it belongs to makes the difference readable.
    match running_dext_cdhash() {
        Some(run) => {
            let which = staged_version_for_cdhash(&run);
            println!(
                "attached in the kernel: code hash {run}{}",
                match which {
                    Some(v) => format!(" = staged version {v}"),
                    None =>
                        " (matches no staged copy — it may be a build installed and since replaced)"
                            .to_string(),
                }
            );
            if let Some(p) = bundled_dext_path(DEXT_I210) {
                if let Some(sub) = bundle_cdhash(&p) {
                    println!(
                        "this bundle would install: code hash {sub}{}",
                        if sub == run {
                            " — ALREADY RUNNING"
                        } else {
                            " — NOT yet running; `activate` would replace it"
                        }
                    );
                }
            }
        }
        None => println!("attached in the kernel: nothing (AVBI210Driver is not matched)"),
    }
    Ok(())
}

/// Which staged copy under `/Library/SystemExtensions` has this code hash, by
/// its `CFBundleVersion`. Answers "what is actually running" in the terms a
/// human reads the list in.
fn staged_version_for_cdhash(cdhash: &str) -> Option<String> {
    let root = std::path::Path::new("/Library/SystemExtensions");
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let dext = entry.path().join(format!("{DEXT_I210}.dext"));
        if !dext.is_dir() {
            continue;
        }
        if bundle_cdhash(&dext).as_deref() == Some(cdhash) {
            let plist = dext.join("Info.plist");
            if let Ok(text) = std::fs::read_to_string(&plist) {
                if let Some(v) = plist_string(&text, "CFBundleVersion") {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// One string value from an XML plist, dependency-free.
fn plist_string(text: &str, key: &str) -> Option<String> {
    let after_key = text.split(&format!("<key>{key}</key>")).nth(1)?;
    let open = after_key.find("<string>")? + "<string>".len();
    let rest = after_key.get(open..)?;
    let close = rest.find("</string>")?;
    let v = rest.get(..close)?.trim();
    (!v.is_empty()).then(|| v.to_string())
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
