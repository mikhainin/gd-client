// Qt6/QML UI for the g-client Google Drive sync suite: lists sync folders
// managed by gdrived, and lets the user add/remove/enable/disable them.
// All state comes from gdrived over D-Bus via the SyncManager QObject
// (see ../src/cxxqt_object.rs); this file only renders it.
//
// Deliberately avoids QtQuick.Layouts (not always packaged alongside the
// base QtQuick Controls modules on every distro) in favour of plain
// Row/Column with explicit widths.

import QtQuick
import QtQuick.Controls
import QtQuick.Window

// This must match the uri specified in the qml_module in build.rs.
import org.gclient.gdrive_ui

ApplicationWindow {
    id: root
    width: 720
    height: 480
    minimumWidth: 480
    minimumHeight: 320
    visible: true
    title: qsTr("g-client - Google Drive Sync")
    color: palette.window

    SyncManager {
        id: syncManager
        Component.onCompleted: refresh()
    }

    // Parsed once per foldersJson change rather than in every binding.
    property var folders: {
        try {
            return JSON.parse(syncManager.foldersJson)
        } catch (e) {
            return []
        }
    }

    header: ToolBar {
        Row {
            anchors.fill: parent
            anchors.margins: 8
            spacing: 8

            Label {
                text: qsTr("Sync folders")
                font.bold: true
                width: root.width - 340
                anchors.verticalCenter: parent.verticalCenter
            }

            Label {
                text: syncManager.connected ? qsTr("Connected to gdrived") : qsTr("Not connected")
                color: syncManager.connected ? "green" : "red"
                anchors.verticalCenter: parent.verticalCenter
            }

            Button {
                text: qsTr("Refresh")
                onClicked: syncManager.refresh()
            }

            Button {
                text: qsTr("Add folder\u2026")
                onClicked: addDialog.open()
            }
        }
    }

    Column {
        anchors.fill: parent
        anchors.margins: 8
        spacing: 8

        Label {
            text: syncManager.statusMessage || ""
            width: parent.width
            wrapMode: Text.WordWrap
        }

        ListView {
            id: folderList
            width: parent.width
            height: parent.height - 30
            clip: true
            model: root.folders
            spacing: 4

            delegate: Frame {
                width: folderList.width

                Row {
                    anchors.fill: parent
                    spacing: 8

                    Column {
                        width: parent.width - 220
                        spacing: 2

                        Label {
                            text: modelData.displayName
                            font.bold: true
                        }
                        Label {
                            text: modelData.localPath
                            font.pixelSize: 11
                            opacity: 0.7
                        }
                    }

                    Label {
                        text: modelData.running ? qsTr("syncing") : (modelData.enabled ? qsTr("starting\u2026") : qsTr("paused"))
                        color: modelData.running ? "green" : (modelData.enabled ? "orange" : "gray")
                        anchors.verticalCenter: parent.verticalCenter
                    }

                    Switch {
                        checked: modelData.enabled
                        anchors.verticalCenter: parent.verticalCenter
                        onToggled: syncManager.setFolderEnabled(modelData.id, checked)
                    }

                    Button {
                        text: qsTr("Remove")
                        anchors.verticalCenter: parent.verticalCenter
                        onClicked: syncManager.removeFolder(modelData.id)
                    }
                }
            }

            Label {
                anchors.centerIn: parent
                visible: folderList.count === 0
                text: qsTr("No sync folders configured yet. Click \"Add folder\u2026\" to create one.")
                opacity: 0.6
            }
        }
    }

    Dialog {
        id: addDialog
        title: qsTr("Add sync folder")
        modal: true
        standardButtons: Dialog.Ok | Dialog.Cancel
        anchors.centerIn: parent
        width: Math.min(420, root.width - 40)

        onAccepted: {
            syncManager.addFolder(
                displayNameField.text,
                driveFolderIdField.text,
                localPathField.text,
                ownerUserField.text,
                ownerGroupField.text,
                true)
            displayNameField.text = ""
            driveFolderIdField.text = ""
            localPathField.text = ""
        }

        Column {
            width: parent.width
            spacing: 6

            Label { text: qsTr("Display name") }
            TextField { id: displayNameField; width: parent.width }

            Label { text: qsTr("Drive folder id (leave empty for \"My Drive\" root)") }
            TextField { id: driveFolderIdField; width: parent.width }

            Label { text: qsTr("Local path") }
            TextField { id: localPathField; width: parent.width; placeholderText: qsTr("/home/user/GoogleDrive/Folder") }

            Label { text: qsTr("Owner user (optional)") }
            TextField { id: ownerUserField; width: parent.width }

            Label { text: qsTr("Owner group (optional)") }
            TextField { id: ownerGroupField; width: parent.width }
        }
    }
}
