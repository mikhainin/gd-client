use cxx_qt_lib::{QByteArray, QGuiApplication, QVector};
use std::os::unix::ffi::OsStrExt;

#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("gdrive-ui/src/application.h");
        include!("cxx-qt-lib/core/qvector/qvector_QByteArray.h");
        type QVector_QByteArray = cxx_qt_lib::QVector<cxx_qt_lib::QByteArray>;
        type QGuiApplication = cxx_qt_lib::QGuiApplication;

        #[rust_name = "create_application"]
        fn createWidgetApplication(args: &QVector_QByteArray) -> UniquePtr<QGuiApplication>;

        #[allow(dead_code)]
        #[rust_name = "verify_tray_menu"]
        fn verifyTrayMenu() -> bool;
    }
}

pub fn new() -> cxx::UniquePtr<QGuiApplication> {
    let mut args = QVector::<QByteArray>::default();
    for arg in std::env::args_os() {
        args.append(QByteArray::from(arg.as_bytes()));
    }
    ffi::create_application(&args)
}

#[allow(dead_code)]
pub fn verify_tray_menu() -> bool {
    ffi::verify_tray_menu()
}
