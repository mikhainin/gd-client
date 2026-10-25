#[cxx::bridge]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("gdrive-ui/src/app_icon.h");
        type QString = cxx_qt_lib::QString;
        fn configureApplicationIcon() -> bool;
        #[allow(dead_code)]
        fn verifyApplicationIcons(preview_directory: &QString) -> bool;
    }
}

pub fn configure() {
    if !ffi::configureApplicationIcon() {
        tracing::error!("could not decode the embedded application SVG; install the Qt SVG runtime plugin (Debian/Ubuntu: libqt6svg6)");
    }
}
