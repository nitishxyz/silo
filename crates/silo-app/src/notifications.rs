//! Notifications (Phase 7 §6). Servers broadcast `block.attention`; this
//! device decides: Settings › Notifications (on/off, per reason, only for
//! blocks not on screen) and each host's switch in Settings › Hosts. A
//! block the user is looking at never notifies.
//!
//! Delivery is `UNUserNotificationCenter`: permission is asked at the
//! first notification the policy would show, never at launch. The title is
//! the trusted label (`host · session · block`), program text goes in the
//! subtitle and body, cleaned. A click opens that host, session and block;
//! a notification whose record was seen anywhere is removed.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use silo_proto::{
    control::AttentionReason,
    model::{BlockStatus, StatusState},
    text::{MAX_BODY, MAX_TITLE, untrusted},
};

use crate::{block_status, hosts::NotifyPrefs};

/// How much of a block the user sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// The focused block of the key Silo window.
    Looking,
    /// In the shown tab of a Silo window while Silo is frontmost.
    Visible,
    Hidden,
}

/// Whether `reason` from a host whose notifications are `host_on` is shown.
#[must_use]
pub fn wanted(prefs: &NotifyPrefs, host_on: bool, reason: AttentionReason, seen: Seen) -> bool {
    let reason_on = match reason {
        AttentionReason::Blocked => prefs.blocked,
        AttentionReason::Done => prefs.done,
        AttentionReason::Error => prefs.error,
        AttentionReason::Notification => prefs.program,
        AttentionReason::Unknown => false,
    };
    prefs.on
        && host_on
        && reason_on
        && match seen {
            Seen::Looking => false,
            Seen::Visible => !prefs.only_hidden,
            Seen::Hidden => true,
        }
}

/// Where a click leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// [`crate::hosts::HostInfo::key`].
    pub host: String,
    pub session_id: Option<String>,
    pub block_id: Option<String>,
}

/// One notification to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub target: Target,
}

/// The state whose record a reason's notification stands for, if any.
fn state_of(reason: AttentionReason) -> Option<StatusState> {
    match reason {
        AttentionReason::Blocked => Some(StatusState::Blocked),
        AttentionReason::Done => Some(StatusState::Done),
        AttentionReason::Error => Some(StatusState::Error),
        AttentionReason::Notification | AttentionReason::Unknown => None,
    }
}

fn reason_key(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::Blocked => "blocked",
        AttentionReason::Done => "done",
        AttentionReason::Error => "error",
        AttentionReason::Notification | AttentionReason::Unknown => "notification",
    }
}

/// `host/block/reason`: one notification per block and state, replaced by
/// the next (the event names no record).
#[must_use]
pub fn identifier(host: &str, block: &str, reason: AttentionReason) -> String {
    format!("{host}/{block}/{}", reason_key(reason))
}

/// An approval's notification.
#[must_use]
pub fn approval_identifier(host: &str, id: &str) -> String {
    format!("{host}/approval/{id}")
}

/// What a `block.attention` shows. Every program-supplied string is
/// cleaned again; the title is trusted (host name, session, block).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn attention_note(
    host_name: &str,
    host: &str,
    session_id: &str,
    session_name: &str,
    block_id: &str,
    block_title: &str,
    reason: AttentionReason,
    title: &str,
    body: &str,
) -> Note {
    let fallback = match reason {
        AttentionReason::Blocked => "Needs you",
        AttentionReason::Done => "Done",
        AttentionReason::Error => "Error",
        AttentionReason::Notification | AttentionReason::Unknown => "Notification",
    };
    let subtitle = untrusted(title, MAX_TITLE);
    let body = untrusted(body, MAX_BODY);
    let (subtitle, body) = match (subtitle.is_empty(), body.is_empty()) {
        (true, true) => (String::new(), fallback.to_owned()),
        (true, false) => (fallback.to_owned(), body),
        (false, _) => (subtitle, body),
    };
    Note {
        id: identifier(host, block_id, reason),
        title: block_status::trusted_label(host_name, session_name, block_title),
        subtitle,
        body,
        target: Target { host: host.to_owned(), session_id: Some(session_id.to_owned()), block_id: Some(block_id.to_owned()) },
    }
}

/// Delivered notifications of `host`'s block `status` whose record is gone
/// (seen on any device, or replaced): to remove.
#[must_use]
pub fn stale(delivered: &HashMap<String, Instant>, host: &str, status: &BlockStatus) -> Vec<String> {
    [AttentionReason::Blocked, AttentionReason::Done, AttentionReason::Error]
        .into_iter()
        .filter(|reason| state_of(*reason).is_some_and(|state| !block_status::has(status, state)))
        .map(|reason| identifier(host, &status.block_id, reason))
        .filter(|id| delivered.contains_key(id))
        .collect()
}

/// How long a block's notification for one reason stands for any repeat.
pub const REPEAT_AFTER: Duration = Duration::from_secs(10);

/// Whether notification `id` is still showing from less than
/// [`REPEAT_AFTER`] ago: a program that keeps raising attention (up to
/// once a second, the server's pace) does not alert again each time.
#[must_use]
pub fn repeat(delivered: &HashMap<String, Instant>, id: &str, now: Instant) -> bool {
    delivered.get(id).is_some_and(|at| now.saturating_duration_since(*at) < REPEAT_AFTER)
}

/// Where a clicked notification's `userInfo` leads.
#[must_use]
pub fn target(host: Option<String>, session_id: Option<String>, block_id: Option<String>) -> Option<Target> {
    let host = host.filter(|h| !h.is_empty())?;
    let nonempty = |s: Option<String>| s.filter(|s| !s.is_empty());
    Some(Target { host, session_id: nonempty(session_id), block_id: nonempty(block_id) })
}

pub use platform::{deliver, denied, install, open_system_settings, remove, set_badge};

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    };

    use block2::RcBlock;
    use objc2::{
        MainThreadMarker, MainThreadOnly, class, define_class, msg_send,
        rc::Retained,
        runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_foundation::{NSArray, NSBundle, NSDictionary, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions, UNNotificationRequest,
        UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };
    use tokio::sync::mpsc::UnboundedSender;

    use super::{Note, Target};

    const UNKNOWN: u8 = 0;
    const GRANTED: u8 = 1;
    const DENIED: u8 = 2;
    static AUTH: AtomicU8 = AtomicU8::new(UNKNOWN);
    static CLICKS: OnceLock<UnboundedSender<Target>> = OnceLock::new();

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SiloNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, completion: &block2::DynBlock<dyn Fn()>) {
                clicked(response);
                completion.call(());
            }

            // Silo frontmost (another block on screen): show it anyway;
            // the policy already decided.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                completion.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }
        }
    );

    /// Notifications need a bundle; a bare dev binary skips them (said once).
    fn bundled() -> bool {
        static BUNDLED: OnceLock<bool> = OnceLock::new();
        *BUNDLED.get_or_init(|| {
            let id = NSBundle::mainBundle().bundleIdentifier();
            if id.is_none() {
                tracing::info!("notifications: not running from an app bundle; none are shown");
            }
            id.is_some()
        })
    }

    fn center() -> Option<Retained<UNUserNotificationCenter>> {
        bundled().then(UNUserNotificationCenter::currentNotificationCenter)
    }

    fn string(dict: &NSDictionary, key: &str) -> Option<String> {
        let key = NSString::from_str(key);
        let key: &AnyObject = &key;
        let value = dict.objectForKey(key)?;
        value.downcast::<NSString>().ok().map(|s| s.to_string())
    }

    fn clicked(response: &UNNotificationResponse) {
        let info = response.notification().request().content().userInfo();
        let target = super::target(string(&info, "host"), string(&info, "session_id"), string(&info, "block_id"));
        tracing::info!(?target, "notification clicked");
        if let (Some(target), Some(clicks)) = (target, CLICKS.get()) {
            let _ = clicks.send(target);
        }
    }

    /// Become the notification center's delegate (before launch finishes,
    /// so a click that launched the app arrives); clicks go to `clicks`.
    pub fn install(clicks: UnboundedSender<Target>) {
        let _ = CLICKS.set(clicks);
        let (Some(center), Some(mtm)) = (center(), MainThreadMarker::new()) else { return };
        let delegate: Retained<Delegate> = unsafe { msg_send![super(Delegate::alloc(mtm).set_ivars(())), init] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The center holds its delegate weakly: it lives as long as the app.
        std::mem::forget(delegate);
    }

    /// Notifications were refused in System Settings (known once asked).
    pub fn denied() -> bool {
        AUTH.load(Ordering::Acquire) == DENIED
    }

    fn content(note: &Note) -> Retained<UNMutableNotificationContent> {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&note.title));
        content.setSubtitle(&NSString::from_str(&note.subtitle));
        content.setBody(&NSString::from_str(&note.body));
        content.setThreadIdentifier(&NSString::from_str(&note.target.host));
        let fields = [
            ("host", Some(note.target.host.as_str())),
            ("session_id", note.target.session_id.as_deref()),
            ("block_id", note.target.block_id.as_deref()),
        ];
        let (keys, values): (Vec<_>, Vec<_>) = fields.iter().filter_map(|(k, v)| Some((NSString::from_str(k), NSString::from_str((*v)?)))).unzip();
        let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let values: Vec<&NSString> = values.iter().map(|v| &**v).collect();
        let info = NSDictionary::from_slices(&keys, &values);
        // SAFETY: an NSDictionary of strings is an NSDictionary of objects.
        let info: Retained<NSDictionary> = unsafe { Retained::cast_unchecked(info) };
        // SAFETY: keys and values are property-list strings.
        unsafe { content.setUserInfo(&info) };
        content
    }

    /// Show `note`, asking for permission the first time.
    pub fn deliver(note: Note) {
        let Some(center) = center() else { return };
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&note.id), &content(&note), None);
        let id = note.id.clone();
        let add = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            let granted = granted.as_bool();
            let before = AUTH.swap(if granted { GRANTED } else { DENIED }, Ordering::AcqRel);
            if !granted {
                if before != DENIED {
                    tracing::info!("notifications are turned off for Silo in System Settings");
                }
                return;
            }
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.addNotificationRequest_withCompletionHandler(&request, None);
            tracing::debug!(id, "notification delivered");
        });
        // Asks only the first time; afterwards it answers at once.
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
            &add,
        );
    }

    /// Remove delivered notifications.
    pub fn remove(ids: Vec<String>) {
        let Some(center) = center().filter(|_| !ids.is_empty()) else { return };
        tracing::debug!(?ids, "notifications removed");
        let ids: Vec<Retained<NSString>> = ids.iter().map(|id| NSString::from_str(id)).collect();
        let ids = NSArray::from_retained_slice(&ids);
        center.removeDeliveredNotificationsWithIdentifiers(&ids);
        center.removePendingNotificationRequestsWithIdentifiers(&ids);
    }

    /// The Dock icon's badge: `count`, none at 0.
    pub fn set_badge(count: u32) {
        static LAST: AtomicU8 = AtomicU8::new(0);
        static SET: AtomicBool = AtomicBool::new(false);
        let shown = u8::try_from(count.min(255)).unwrap_or(u8::MAX);
        if SET.swap(true, Ordering::AcqRel) && LAST.swap(shown, Ordering::AcqRel) == shown {
            return;
        }
        LAST.store(shown, Ordering::Release);
        tracing::debug!(count, "dock badge");
        let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
        // SAFETY: AppKit on the main thread (the app's UI thread).
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let tile: *mut AnyObject = msg_send![app, dockTile];
            let _: () = msg_send![tile, setBadgeLabel: label.as_deref()];
        }
    }

    /// System Settings › Notifications.
    pub fn open_system_settings() {
        let url = "x-apple.systempreferences:com.apple.Notifications-Settings.extension";
        if let Err(error) = std::process::Command::new("/usr/bin/open").arg(url).spawn() {
            tracing::warn!("open notification settings: {error}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Note, Target};

    pub fn install(_clicks: tokio::sync::mpsc::UnboundedSender<Target>) {}
    pub fn denied() -> bool {
        false
    }
    pub fn deliver(_note: Note) {}
    pub fn remove(_ids: Vec<String>) {}
    pub fn set_badge(_count: u32) {}
    pub fn open_system_settings() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_proto::model::ProgramStatus;

    #[test]
    fn policy_follows_settings_hosts_and_what_is_on_screen() {
        let on = NotifyPrefs::default();
        let r = AttentionReason::Blocked;
        assert!(wanted(&on, true, r, Seen::Hidden));
        assert!(!wanted(&on, true, r, Seen::Looking), "never the block being looked at");
        assert!(!wanted(&on, true, r, Seen::Visible), "only when not on screen, by default");
        let anywhere = NotifyPrefs { only_hidden: false, ..on };
        assert!(wanted(&anywhere, true, r, Seen::Visible));
        assert!(!wanted(&anywhere, true, r, Seen::Looking), "not even then");
        assert!(!wanted(&on, false, r, Seen::Hidden), "the host's switch is off");
        assert!(!wanted(&NotifyPrefs { on: false, ..on }, true, r, Seen::Hidden));
        let no_done = NotifyPrefs { done: false, ..on };
        assert!(!wanted(&no_done, true, AttentionReason::Done, Seen::Hidden));
        assert!(wanted(&no_done, true, AttentionReason::Error, Seen::Hidden));
        assert!(!wanted(&NotifyPrefs { program: false, ..on }, true, AttentionReason::Notification, Seen::Hidden));
        assert!(!wanted(&on, true, AttentionReason::Unknown, Seen::Hidden));
    }

    #[test]
    fn notes_are_labelled_and_cleaned() {
        let note = attention_note(
            "mini",
            "e1",
            "s1",
            "work",
            "b1",
            "api\u{202e}",
            AttentionReason::Blocked,
            "claude\u{200b} wants\nto run",
            "rm \u{1b}[31m-rf",
        );
        assert_eq!(note.id, "e1/b1/blocked");
        assert_eq!(note.title, "mini · work · api");
        assert_eq!(note.subtitle, "claude wants to run");
        assert_eq!(note.body, "rm [31m-rf");
        assert_eq!(note.target, Target { host: "e1".into(), session_id: Some("s1".into()), block_id: Some("b1".into()) });
        let bare = attention_note("This Mac", "local", "s1", "work", "b1", "zsh", AttentionReason::Done, "", "");
        assert_eq!((bare.subtitle.as_str(), bare.body.as_str()), ("", "Done"));
        let body_only = attention_note("This Mac", "local", "s1", "work", "b1", "zsh", AttentionReason::Notification, "", "build ok");
        assert_eq!(
            (body_only.subtitle.as_str(), body_only.body.as_str(), body_only.id.as_str()),
            ("Notification", "build ok", "local/b1/notification")
        );
    }

    #[test]
    fn seen_records_take_their_notifications_away() {
        let now = Instant::now();
        let delivered: HashMap<String, Instant> = ["local/b1/done", "local/b1/blocked", "local/b2/error"].map(|id| (id.to_owned(), now)).into();
        let record = |state| ProgramStatus {
            id: String::new(),
            state,
            kind: None,
            progress: None,
            app: String::new(),
            title: String::new(),
            message: String::new(),
            updated_at: 0,
        };
        let status = BlockStatus { block_id: "b1".into(), records: vec![record(StatusState::Blocked)], progress: None, busy: None };
        assert_eq!(stale(&delivered, "local", &status), ["local/b1/done"]);
        let empty = BlockStatus { records: Vec::new(), ..status };
        let mut gone = stale(&delivered, "local", &empty);
        gone.sort();
        assert_eq!(gone, ["local/b1/blocked", "local/b1/done"]);
        assert!(stale(&delivered, "e1", &empty).is_empty(), "another host's");
    }

    #[test]
    fn a_showing_notification_is_not_repeated_for_a_while() {
        let at = Instant::now();
        let delivered: HashMap<String, Instant> = [("local/b1/notification".to_owned(), at)].into();
        assert!(repeat(&delivered, "local/b1/notification", at + Duration::from_secs(1)));
        assert!(!repeat(&delivered, "local/b1/notification", at + REPEAT_AFTER), "once it has stood a while");
        assert!(!repeat(&delivered, "local/b1/done", at), "another reason");
        assert!(!repeat(&HashMap::new(), "local/b1/notification", at), "taken away (seen): shown again");
    }

    #[test]
    fn clicks_route_by_user_info() {
        assert_eq!(target(None, Some("s".into()), None), None);
        assert_eq!(target(Some(String::new()), None, None), None);
        assert_eq!(
            target(Some("e1".into()), Some("s1".into()), Some(String::new())),
            Some(Target { host: "e1".into(), session_id: Some("s1".into()), block_id: None })
        );
    }
}
