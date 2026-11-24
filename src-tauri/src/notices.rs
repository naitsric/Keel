//! Tells the person, through macOS's notifications, what changed while Keel wasn't in front: what
//! to say is the engine's (`NoticeWatch`), and this only sends it. A click opens what it's about.
//! Only from the app as macOS knows it, an `.app` with its identifier: run straight from a build
//! folder, there's no one to send them as, and nothing is sent.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use keel_engine::{Engine, Notice, NoticeWatch};
use keel_store::Store;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// What a click on a notice opens, for the window to show.
pub const OPEN_EVENT: &str = "keel://open";
/// A pause or a pull request can change without a run saying so, so Keel looks now and then too.
const LOOK_AGAIN: Duration = Duration::from_secs(30);
/// Changes come in bursts. Waiting a moment makes them one look.
const SETTLE: Duration = Duration::from_millis(300);

/// A note for the watch that something changed. Any number of them before it looks count as one.
#[derive(Default)]
pub struct Poke {
    pending: Mutex<bool>,
    signal: Condvar,
}

impl Poke {
    pub fn notify(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signal.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (mut pending, _) = self
            .signal
            .wait_timeout_while(pending, timeout, |pending| !*pending)
            .unwrap_or_else(|e| e.into_inner());
        *pending = false;
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Open {
    /// None for the board.
    run_id: Option<String>,
}

/// Starts watching. Call it once runs left from the last session are back, or what they became
/// meanwhile would be said as news.
pub fn start(app: AppHandle, engine: Engine, store: Arc<Store>, poke: Arc<Poke>) {
    if !mac::available() {
        return;
    }
    let opener = app.clone();
    mac::init(move |run_id| open(&opener, run_id));
    std::thread::spawn(move || {
        let mut watch = NoticeWatch::default();
        loop {
            if let Ok(notices) = watch.look(&engine, &language(&store)) {
                // What's on screen already says it.
                if !in_front(&app) {
                    for notice in &notices {
                        mac::send(notice);
                    }
                }
            }
            poke.wait(LOOK_AGAIN);
            std::thread::sleep(SETTLE);
        }
    });
}

fn in_front(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|window| window.is_focused().ok())
        .unwrap_or(false)
}

fn open(app: &AppHandle, run_id: Option<String>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    let _ = app.emit(OPEN_EVENT, Open { run_id });
}

/// The one the person chose in Keel, or else the system's, as the window picks it.
fn language(store: &Store) -> String {
    match store.setting(crate::LANGUAGE_KEY).ok().flatten().as_deref() {
        Some(chosen @ ("es" | "en")) => chosen.to_owned(),
        _ if mac::system_language().is_some_and(|l| l.to_lowercase().starts_with("es")) => {
            "es".to_owned()
        }
        _ => "en".to_owned(),
    }
}

/// Which run a notice opens, in its identifier, so a click can find it.
fn identifier(notice: &Notice, number: u64) -> String {
    match &notice.run_id {
        Some(run_id) => format!("run:{run_id}:{number}"),
        None => format!("board:{number}"),
    }
}

/// What a clicked notice opens. None for the board.
fn opens(identifier: &str) -> Option<String> {
    let run = identifier.strip_prefix("run:")?;
    let (run_id, _) = run.rsplit_once(':')?;
    Some(run_id.to_owned())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{AllocAnyThread, define_class, msg_send};
    use objc2_foundation::{NSBundle, NSError, NSLocale, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };

    use super::{Notice, identifier, opens};

    type Opener = Box<dyn Fn(Option<String>) + Send + Sync>;
    static OPENER: OnceLock<Opener> = OnceLock::new();
    static SENT: AtomicU64 = AtomicU64::new(0);

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and this doesn't implement Drop.
        #[unsafe(super = NSObject)]
        #[name = "KeelNoticeDelegate"]
        struct Delegate;

        // SAFETY: NSObjectProtocol has no safety requirements.
        unsafe impl NSObjectProtocol for Delegate {}

        // SAFETY: the signature is the protocol's.
        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                completion_handler: &block2::DynBlock<dyn Fn()>,
            ) {
                let identifier = response.notification().request().identifier().to_string();
                if let Some(open) = OPENER.get() {
                    open(opens(&identifier));
                }
                completion_handler.call(());
            }
        }
    );

    impl Delegate {
        fn new() -> Retained<Self> {
            // SAFETY: NSObject's init.
            unsafe { msg_send![Self::alloc(), init] }
        }
    }

    /// macOS only has a notification center for an app it knows: one inside an `.app`, with an
    /// identifier. Asking for it from anything else ends the process.
    pub fn available() -> bool {
        let bundle = NSBundle::mainBundle();
        bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
    }

    pub fn system_language() -> Option<String> {
        NSLocale::preferredLanguages()
            .firstObject()
            .map(|language| language.to_string())
    }

    /// Asks the person once whether Keel may send notices, and listens for clicks.
    pub fn init(open: impl Fn(Option<String>) + Send + Sync + 'static) {
        if OPENER.set(Box::new(open)).is_err() {
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        // The center holds its delegate weakly, so this one stays for as long as the app.
        let delegate = Delegate::new();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        std::mem::forget(delegate);
        let answered = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &answered,
        );
    }

    /// Without leave from the person, macOS drops it, and there's nothing more to do.
    pub fn send(notice: &Notice) {
        let number = SENT.fetch_add(1, Ordering::Relaxed);
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&notice.title));
        content.setBody(&NSString::from_str(&notice.body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        // A run's notices stack together.
        content.setThreadIdentifier(&NSString::from_str(
            notice.run_id.as_deref().unwrap_or("board"),
        ));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&identifier(notice, number)),
            &content,
            None,
        );
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, None);
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    use super::Notice;

    pub fn available() -> bool {
        false
    }

    pub fn system_language() -> Option<String> {
        None
    }

    pub fn init(_open: impl Fn(Option<String>) + Send + Sync + 'static) {}

    pub fn send(_notice: &Notice) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_opens_the_run_a_notice_is_about_or_the_board() {
        let about = |run_id: Option<&str>| Notice {
            run_id: run_id.map(str::to_owned),
            title: String::new(),
            body: String::new(),
        };
        let run = identifier(&about(Some("5e88-0d0f")), 3);
        assert_eq!(opens(&run).as_deref(), Some("5e88-0d0f"));
        assert_eq!(opens(&identifier(&about(None), 4)), None);
        assert_eq!(opens("something else"), None);
    }
}
