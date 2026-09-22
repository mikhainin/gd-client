//! Blocking D-Bus client used by the Qt/QML UI to talk to the `gdrived`
//! background service. A new connection is opened per call, which keeps the
//! Rust-side `SyncManager` `QObject` free of any async runtime -
//! `zbus::blocking` drives its own lightweight event loop internally, so
//! this stays simple to call from `#[qinvokable]` methods.
//!
//! `#[qinvokable]` methods run on the Qt/QML UI thread, so every call here
//! must be bounded: `zbus::blocking` has no built-in call timeout, and a
//! stalled session bus connection or an unresponsive `gdrived` (e.g. stuck
//! on a hung Drive API request - see `gdrive-sync::drive_client`'s own
//! timeouts) would otherwise freeze the whole UI with no way to recover
//! short of killing the process. See AGENTS.md: every network/IPC call
//! must have a timeout.

use std::sync::mpsc;
use std::time::Duration;

use gdrive_common::dbus_api::GDrive1ProxyBlocking;

/// Upper bound on a single D-Bus round-trip to `gdrived`, as observed from
/// the UI thread. Generous enough for normal operations (including
/// `gdrived` relaying a Drive API call) while still guaranteeing the UI
/// thread is never blocked indefinitely.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Opens a session bus connection to `gdrived` and runs `f` against a proxy
/// for its `org.gclient.GDrive1` interface, returning a human-readable error
/// string (rather than `zbus::Error`) so it's trivial to surface in the UI.
///
/// `f` runs on a dedicated worker thread so that, even if it hangs (e.g. a
/// stuck session bus connection), the calling (UI) thread only ever waits
/// up to [`CALL_TIMEOUT`] before getting an error back; the worker thread
/// itself is abandoned (and reclaimed by the OS once it eventually
/// finishes) rather than forcibly killed, since Rust has no safe way to
/// cancel a running thread.
pub fn with_proxy<T>(
    f: impl FnOnce(&GDrive1ProxyBlocking<'_>) -> zbus::Result<T> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let result = (|| {
            let connection = zbus::blocking::Connection::session()
                .map_err(|e| format!("could not connect to the session D-Bus: {e}"))?;
            let proxy = GDrive1ProxyBlocking::new(&connection)
                .map_err(|e| format!("could not reach gdrived on the session bus: {e}"))?;
            f(&proxy).map_err(|e| e.to_string())
        })();
        // The receiver may already have timed out and been dropped; that's
        // fine, there's simply no one left to deliver the (late) result to.
        let _ = tx.send(result);
    });

    rx.recv_timeout(CALL_TIMEOUT).unwrap_or_else(|_| {
        Err(format!(
            "timed out waiting {CALL_TIMEOUT:?} for a reply from gdrived over D-Bus"
        ))
    })
}

/// Asks the freedesktop desktop portal (`org.freedesktop.portal.Desktop`,
/// implemented by `xdg-desktop-portal-{kde,gnome,gtk,...}` on effectively
/// every modern desktop environment) whether the user prefers a dark color
/// scheme, per the `org.freedesktop.appearance` `color-scheme` setting
/// (0 = no preference, 1 = prefer dark, 2 = prefer light). Used instead of
/// relying on Qt's own platform-theme integration (which requires an extra,
/// not-always-installed Qt6 platform theme plugin) so the UI's dark-mode
/// support doesn't depend on how a given distro packages Qt. Defaults to
/// `false` (light) if the portal is unavailable or the call fails.
pub fn system_prefers_dark() -> bool {
    (|| -> zbus::Result<bool> {
        let connection = zbus::blocking::Connection::session()?;
        let reply = connection.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            "Read",
            &("org.freedesktop.appearance", "color-scheme"),
        )?;
        // `Read` returns a `v` wrapping the setting's own value, which for
        // `color-scheme` is itself a `u` - i.e. a variant nested one level
        // deeper than usual, hence the extra `downcast`.
        let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
        let color_scheme: u32 = zbus::zvariant::Value::from(value).downcast()?;
        Ok(color_scheme == 1)
    })()
    .unwrap_or(false)
}
