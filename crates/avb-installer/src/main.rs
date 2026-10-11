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
        fn did_fail(&self, _request: &OSSystemExtensionRequest, error: &NSError) {
            let code = error.code();
            eprintln!(
                "request failed: {} (domain {}, code {} = {})",
                error.localizedDescription(),
                error.domain(),
                code,
                error_code_name(code)
            );
            std::process::exit(2);
        }
    }
);

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
