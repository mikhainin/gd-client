#[path = "../src/app_icon.rs"]
mod app_icon;
// QML module registration also links the SyncManager bridge.
#[path = "../src/cxxqt_object.rs"]
pub mod cxxqt_object;
#[path = "../src/dbus_client.rs"]
mod dbus_client;

use cxx_qt_lib::{QGuiApplication, QString};

fn main() {
    std::env::set_var("QT_QPA_PLATFORM", "offscreen");
    let _app = QGuiApplication::new();
    app_icon::configure();
    let directory = std::env::var("GDRIVE_ICON_PREVIEWS").unwrap_or_default();
    assert!(
        app_icon::ffi::verifyApplicationIcons(&QString::from(directory)),
        "Embedded SVG decoding/window identity failed; the Qt SVG runtime plugin (libqt6svg6) is required"
    );
}
