//! Blocking D-Bus client used by the Qt/QML UI to talk to the `gdrived`
//! background service. A new connection is opened per call, which keeps the
//! Rust-side `SyncManager` `QObject` free of any async runtime -
//! `zbus::blocking` drives its own lightweight event loop internally, so
//! this stays simple to call from `#[qinvokable]` methods.

use gdrive_common::dbus_api::GDrive1ProxyBlocking;

/// Opens a session bus connection to `gdrived` and runs `f` against a proxy
/// for its `org.gclient.GDrive1` interface, returning a human-readable error
/// string (rather than `zbus::Error`) so it's trivial to surface in the UI.
pub fn with_proxy<T>(
    f: impl FnOnce(&GDrive1ProxyBlocking<'_>) -> zbus::Result<T>,
) -> Result<T, String> {
    let connection = zbus::blocking::Connection::session()
        .map_err(|e| format!("could not connect to the session D-Bus: {e}"))?;
    let proxy = GDrive1ProxyBlocking::new(&connection)
        .map_err(|e| format!("could not reach gdrived on the session bus: {e}"))?;
    f(&proxy).map_err(|e| e.to_string())
}
