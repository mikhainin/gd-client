#[path = "../src/app_icon.rs"]
mod app_icon;
#[path = "../src/application.rs"]
mod application;
// QML module registration also links the SyncManager bridge.
#[path = "../src/cxxqt_object.rs"]
pub mod cxxqt_object;
#[path = "../src/dbus_client.rs"]
mod dbus_client;
#[path = "../src/local_picker.rs"]
mod local_picker;

use cxx_qt_lib::QString;

fn main() {
    std::env::set_var("QT_QPA_PLATFORM", "offscreen");
    let _app = application::new();
    assert!(
        application::verify_tray_menu(),
        "The widget-backed tray menu must initialize and dispatch its actions"
    );
    app_icon::configure();
    let directory = std::env::var("GDRIVE_ICON_PREVIEWS").unwrap_or_default();
    assert!(
        app_icon::ffi::verifyApplicationIcons(&QString::from(directory)),
        "Embedded SVG decoding/window identity failed; the Qt SVG runtime plugin (libqt6svg6) is required"
    );
}
