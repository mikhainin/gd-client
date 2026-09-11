//! Builds the `SyncManager` `QObject` C++ bridge and the QML module
//! (`org.gclient.gdrive_ui`) containing `qml/main.qml`, then links the
//! result into a plain Cargo binary (no CMake required) - see
//! `cxx-qt`'s `cargo_without_cmake` example, which this mirrors.

use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("org.gclient.gdrive_ui").qml_file("qml/main.qml"))
        .files(["src/cxxqt_object.rs"])
        .build();
}
