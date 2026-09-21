//! Blocking D-Bus client used by the Qt/QML UI to talk to the `gdrived`
//! background service, plus a helper (`system_prefers_dark`) for querying
//! the freedesktop desktop portal's dark-mode setting. A new connection is
//! opened per call, which keeps the Rust-side `SyncManager` `QObject` free
//! of any async runtime - `zbus::blocking` drives its own lightweight event
//! loop internally, so this stays simple to call from `#[qinvokable]`
//! methods.

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
