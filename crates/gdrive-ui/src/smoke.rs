//! Read-only observations of the real application, compiled only for smoke tests.

#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("gdrive-ui/src/smoke.h");
        type QQmlApplicationEngine = cxx_qt_lib::QQmlApplicationEngine;
        type QString = cxx_qt_lib::QString;
        fn observeSmoke(engine: Pin<&mut QQmlApplicationEngine>, path: &QString);
    }
}

pub fn observe(engine: std::pin::Pin<&mut cxx_qt_lib::QQmlApplicationEngine>) {
    if let Ok(path) = std::env::var("GDRIVE_TEST_UI_SNAPSHOT") {
        ffi::observeSmoke(engine, &cxx_qt_lib::QString::from(path.as_str()));
    }
}
