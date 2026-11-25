//! Notifications on macOS that open their chat when clicked.
//!
//! mac-notification-sys's `wait_for_click` parks a thread per notification and
//! polls `deliveredNotifications` on the main thread every half second until
//! that notification leaves Notification Center. Banners that time out stay
//! there, so after a day of notifications the main thread did nothing but fetch
//! every delivered notification over XPC, once per waiter, and kept `usernoted`
//! busy too. Here a notification carries where to go in its `userInfo`, and a
//! delegate opens that on click: nothing waits or polls, and a notification
//! from before a restart still opens its chat.
#![allow(deprecated)] // NSUserNotification: UNUserNotificationCenter needs a signed bundle, and dev builds aren't.

use std::cell::OnceCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSDictionary, NSString, NSUserNotification, NSUserNotificationCenter, NSUserNotificationCenterDelegate};
use tauri::AppHandle;

use crate::windows::Navigate;

const NAV_KEY: &str = "littleguysNavigate";

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LittleguysNotificationDelegate"]
    #[ivars = AppHandle]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:didActivateNotification:))]
        fn did_activate(&self, center: &NSUserNotificationCenter, notification: &NSUserNotification) {
            if let Some(nav) = navigate_of(notification) {
                crate::windows::open_main(self.ivars(), Some(nav));
            }
            center.removeDeliveredNotification(notification);
        }
    }
);

thread_local! {
    // The center holds its delegate weakly; this keeps it alive (main thread only).
    static DELEGATE: OnceCell<Retained<Delegate>> = const { OnceCell::new() };
}

/// Takes clicks on littleguys' notifications from the start, so ones left in
/// Notification Center by an earlier run open their chat too.
pub fn setup(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            center(&handle, mtm);
        }
    });
}

/// Shows a notification; clicking it opens `nav`, when there is one.
pub fn show(app: &AppHandle, title: &str, body: &str, nav: Option<Navigate>) {
    let (handle, title, body) = (app.clone(), title.to_string(), body.to_string());
    let nav = nav.and_then(|n| serde_json::to_string(&n).ok());
    let _ = app.run_on_main_thread(move || {
        let Some(center) = MainThreadMarker::new().and_then(|mtm| center(&handle, mtm)) else {
            log::warn!("notification failed: no notification center");
            return;
        };
        let notification = NSUserNotification::new();
        notification.setTitle(Some(&NSString::from_str(&title)));
        notification.setInformativeText(Some(&NSString::from_str(&body)));
        if let Some(json) = nav {
            let (key, value) = (NSString::from_str(NAV_KEY), NSString::from_str(&json));
            let info = NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[&value as &AnyObject]);
            // SAFETY: a dictionary of strings is a property list, as userInfo requires.
            unsafe { notification.setUserInfo(Some(&info)) };
        }
        center.deliverNotification(&notification);
    });
}

/// The notification center with our delegate on it. Set again each time: the
/// notification plugin's library takes the delegate over when it shows one.
fn center(app: &AppHandle, mtm: MainThreadMarker) -> Option<Retained<NSUserNotificationCenter>> {
    static APP_SET: std::sync::Once = std::sync::Once::new();
    APP_SET.call_once(|| {
        // Same as the plugin: unbundled dev builds borrow Terminal's identity.
        let identifier = app.config().identifier.clone();
        let _ = mac_notification_sys::set_application(if tauri::is_dev() { "com.apple.Terminal" } else { &identifier });
    });
    // nil without a bundle identifier, which the generated binding would panic on.
    let center: Option<Retained<NSUserNotificationCenter>> =
        unsafe { msg_send![NSUserNotificationCenter::class(), defaultUserNotificationCenter] };
    let center = center?;
    DELEGATE.with(|cell| {
        let delegate = cell.get_or_init(|| {
            let this = Delegate::alloc(mtm).set_ivars(app.clone());
            // SAFETY: NSObject's init on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        });
        // SAFETY: DELEGATE keeps the delegate alive for the rest of the process.
        unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&**delegate))) };
    });
    Some(center)
}

fn navigate_of(notification: &NSUserNotification) -> Option<Navigate> {
    let value = notification.userInfo()?.objectForKey(&NSString::from_str(NAV_KEY))?;
    let json = value.downcast::<NSString>().ok()?;
    serde_json::from_str(&json.to_string()).ok()
}
