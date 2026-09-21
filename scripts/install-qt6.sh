#!/usr/bin/env bash
# Installs Qt6 + build tooling required to build the gdrive-ui crate
# (cxx-qt) on Debian/Ubuntu. Run on your actual development machine, not
# inside a restricted sandbox.
set -euo pipefail

sudo apt update
sudo apt install -y \
  qt6-base-dev qt6-declarative-dev qt6-base-dev-tools \
  qml6-module-qtquick qml6-module-qtquick-controls \
  qml6-module-qtquick-templates \
  qml6-module-qtqml-workerscript qml6-module-qtquick-window \
  qml6-module-qt-labs-platform qml6-module-qt-labs-folderlistmodel \
  build-essential cmake ninja-build pkg-config \
  libdbus-1-dev

# Optional: kdialog gives the "Add folder" local path picker a genuine
# native KDE folder-browse dialog. Without it, gdrive-ui falls back to its
# own in-app FolderListModel-based browser, so this is not required.
sudo apt install -y kdialog || true

echo "--- Verifying Qt is discoverable ---"
qmake6 -query QT_INSTALL_PREFIX || pkg-config --modversion Qt6Core
