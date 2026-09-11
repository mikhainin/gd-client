//! `gdrive-ui`: Qt6/QML front-end for configuring and monitoring the
//! `gdrived` background service. All synchronisation state lives in
//! `gdrived`; this binary only talks to it over D-Bus (see
//! `dbus_client`/`cxxqt_object::SyncManager`) and renders `qml/main.qml`.

pub mod cxxqt_object;
mod dbus_client;

use cxx_qt::casting::Upcast;
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQmlEngine, QUrl};
use std::pin::Pin;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from(
            "qrc:/qt/qml/org/gclient/gdrive_ui/qml/main.qml",
        ));
    }

    if let Some(engine) = engine.as_mut() {
        let engine: Pin<&mut QQmlEngine> = engine.upcast_pin();
        engine
            .on_quit(|_| {
                tracing::info!("gdrive-ui quitting");
            })
            .release();
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
