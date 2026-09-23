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
import Qt.labs.folderlistmodel
import Qt.labs.platform as Platform

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

    // Approximation of the Breeze Dark palette, applied via the Binding
    // below when SyncManager.darkMode (detected via the freedesktop desktop
    // portal's color-scheme setting, see ../src/dbus_client.rs) is true.
    // Every Fusion-styled control (see src/main.rs, which sets Fusion as
    // the fallback style) reads its colors from the window's `palette`
    // property, so this alone re-themes the whole UI.
    Palette {
        id: darkPalette
        window: "#31363b"
        windowText: "#eff0f1"
        base: "#232629"
        alternateBase: "#2a2e32"
        text: "#eff0f1"
        button: "#31363b"
        buttonText: "#eff0f1"
        light: "#3d4247"
        midlight: "#3a3f44"
        mid: "#232629"
        dark: "#1b1e21"
        highlight: "#3daee9"
        highlightedText: "#eff0f1"
        placeholderText: "#9099a0"
        toolTipBase: "#232629"
        toolTipText: "#eff0f1"
    }

    // Only overrides root.palette while darkMode is true; restores the
    // Fusion style's own default (light) palette otherwise, avoiding a
    // binding loop that a plain `darkMode ? darkPalette : palette`
    // conditional would create.
    Binding {
        target: root
        property: "palette"
        value: darkPalette
        when: syncManager.darkMode
        restoreMode: Binding.RestoreBindingOrValue
    }

    // Hide to tray instead of quitting when the window is closed; the
    // application only truly exits via the tray menu's "Quit" action.
    onClosing: close => {
        close.accepted = false
        root.hide()
    }

    SyncManager {
        id: syncManager
        Component.onCompleted: refresh()
    }

    // Polls gdrived periodically so folder sync status changes (which have
    // no D-Bus signal of their own) are picked up without requiring the user
    // to click "Refresh" manually. Sign-in completion does not rely on this:
    // gdrived emits AuthenticationChanged, which SyncManager subscribes to.
    // refresh() only queues D-Bus requests, so this never blocks the UI.
    Timer {
        interval: 3000
        running: true
        repeat: true
        onTriggered: syncManager.refresh()
    }

    Platform.SystemTrayIcon {
        id: trayIcon
        visible: true
        icon.name: "folder-remote"
        tooltip: qsTr("g-client - Google Drive Sync")

        onActivated: root.visible ? root.hide() : root.show()

        menu: Platform.Menu {
            Platform.MenuItem {
                text: root.visible ? qsTr("Hide") : qsTr("Show")
                onTriggered: root.visible ? root.hide() : root.show()
            }
            Platform.MenuItem {
                text: qsTr("Quit")
                onTriggered: Qt.quit()
            }
        }
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
                width: root.width - 470
                anchors.verticalCenter: parent.verticalCenter
            }

            Label {
                text: syncManager.connected ? qsTr("Connected to gdrived") : qsTr("Not connected")
                color: syncManager.connected ? "green" : "red"
                anchors.verticalCenter: parent.verticalCenter
            }

            Button {
                text: syncManager.authenticated ? qsTr("Sign out") : qsTr("Sign in\u2026")
                anchors.verticalCenter: parent.verticalCenter
                onClicked: syncManager.authenticated ? syncManager.signOut() : syncManager.signIn()
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

    // Custom local folder browser (rather than QtQuick.Dialogs' FolderDialog)
    // so we can offer a "New folder" button, which Qt doesn't provide out of
    // the box for its Quick-based FolderDialog fallback. Backed by
    // Qt.labs.folderlistmodel (a lightweight live directory listing) and
    // SyncManager.createLocalFolder for folder creation.
    Dialog {
        id: localFolderDialog
        title: qsTr("Select local folder")
        modal: true
        standardButtons: Dialog.Cancel
        anchors.centerIn: parent
        width: Math.min(420, root.width - 40)
        height: Math.min(460, root.height - 40)

        property string currentPath: Platform.StandardPaths.writableLocation(Platform.StandardPaths.HomeLocation)
            .toString().replace("file://", "")
        property bool showNewFolderRow: false

        function goUp() {
            var idx = currentPath.lastIndexOf("/")
            currentPath = idx > 0 ? currentPath.substring(0, idx) : "/"
        }

        function enterFolder(name) {
            currentPath = (currentPath === "/" ? "" : currentPath) + "/" + name
        }

        onOpened: {
            showNewFolderRow = false
            newFolderNameField.text = ""
        }

        FolderListModel {
            id: localFolderModel
            folder: "file://" + localFolderDialog.currentPath
            showFiles: false
            showDotAndDotDot: false
            sortField: FolderListModel.Name
        }

        Column {
            anchors.fill: parent
            spacing: 6

            Row {
                width: parent.width
                spacing: 4

                Button {
                    text: qsTr("\u2191 Up")
                    enabled: localFolderDialog.currentPath !== "/"
                    onClicked: localFolderDialog.goUp()
                }

                Label {
                    text: localFolderDialog.currentPath
                    elide: Text.ElideMiddle
                    width: parent.width - 180
                    anchors.verticalCenter: parent.verticalCenter
                }
            }

            ListView {
                width: parent.width
                height: parent.height - (localFolderDialog.showNewFolderRow ? 170 : 130)
                clip: true
                model: localFolderModel
                ScrollBar.vertical: ScrollBar { policy: ScrollBar.AlwaysOn; width: 14 }

                delegate: ItemDelegate {
                    width: ListView.view.width
                    text: fileName
                    onClicked: localFolderDialog.enterFolder(fileName)
                }

                Label {
                    anchors.centerIn: parent
                    visible: localFolderModel.count === 0
                    text: qsTr("No sub-folders here.")
                    opacity: 0.6
                }
            }

            Row {
                width: parent.width
                visible: localFolderDialog.showNewFolderRow
                spacing: 6

                TextField {
                    id: newFolderNameField
                    width: parent.width - 90
                    placeholderText: qsTr("New folder name")
                    onAccepted: createNewFolderButton.clicked()
                }
                Button {
                    id: createNewFolderButton
                    text: qsTr("Create")
                    width: 84
                    onClicked: {
                        if (syncManager.createLocalFolder(localFolderDialog.currentPath, newFolderNameField.text)) {
                            localFolderDialog.enterFolder(newFolderNameField.text)
                            localFolderDialog.showNewFolderRow = false
                            newFolderNameField.text = ""
                        }
                    }
                }
            }

            Button {
                width: parent.width
                text: qsTr("New folder\u2026")
                visible: !localFolderDialog.showNewFolderRow
                onClicked: localFolderDialog.showNewFolderRow = true
            }

            Button {
                width: parent.width
                text: qsTr("Select this folder")
                onClicked: {
                    localPathField.text = localFolderDialog.currentPath
                    localFolderDialog.close()
                }
            }
        }
    }

    Dialog {
        id: driveFolderDialog
        title: qsTr("Select Drive folder")
        modal: true
        standardButtons: Dialog.Cancel
        anchors.centerIn: parent
        width: Math.min(420, root.width - 40)
        height: Math.min(420, root.height - 40)

        // Stack of {id, name} visited so far, for breadcrumb navigation and
        // the "Select this folder" action; the last entry is the folder
        // currently being browsed ("" id means "My Drive" root).
        property var pathStack: [{ id: "", name: qsTr("My Drive") }]
        property var entries: []
        property bool loading: false

        function currentFolder() {
            return pathStack[pathStack.length - 1]
        }

        // Asks for the current folder's children and returns immediately;
        // SyncManager delivers them via its driveFoldersReady signal (see
        // the Connections below), so browsing never blocks the UI.
        function load() {
            entries = []
            loading = true
            syncManager.listDriveFolders(currentFolder().id)
        }

        function enterFolder(id, name) {
            pathStack = pathStack.concat([{ id: id, name: name }])
            load()
        }

        function goTo(index) {
            pathStack = pathStack.slice(0, index + 1)
            load()
        }

        onOpened: {
            pathStack = [{ id: "", name: qsTr("My Drive") }]
            load()
        }

        Column {
            anchors.fill: parent
            spacing: 6

            Row {
                width: parent.width
                spacing: 4
                Repeater {
                    model: driveFolderDialog.pathStack
                    delegate: Button {
                        text: modelData.name
                        flat: true
                        onClicked: driveFolderDialog.goTo(index)
                    }
                }
            }

            ListView {
                width: parent.width
                height: parent.height - 90
                clip: true
                model: driveFolderDialog.entries
                ScrollBar.vertical: ScrollBar { policy: ScrollBar.AlwaysOn; width: 14 }

                delegate: ItemDelegate {
                    width: ListView.view.width
                    text: modelData.name
                    onClicked: driveFolderDialog.enterFolder(modelData.id, modelData.name)
                }

                Label {
                    anchors.centerIn: parent
                    visible: driveFolderDialog.entries.length === 0
                    text: driveFolderDialog.loading ? qsTr("Loading\u2026")
                                                    : qsTr("No sub-folders here.")
                    opacity: 0.6
                }
            }

            Button {
                width: parent.width
                text: qsTr("Select \"%1\"").arg(driveFolderDialog.currentFolder().name)
                onClicked: {
                    driveFolderIdField.text = driveFolderDialog.currentFolder().id
                    driveFolderPathField.text = qsTr("Selected: %1").arg(
                        driveFolderDialog.pathStack.map(function (entry) {
                            return entry.name
                        }).join(" / "))
                    driveFolderDialog.close()
                }
            }
        }

        // The listing arrives asynchronously, so ignore replies for a
        // folder the user has already navigated away from.
        Connections {
            target: syncManager

            function onDriveFoldersReady(parentId, foldersJson) {
                if (parentId !== driveFolderDialog.currentFolder().id)
                    return

                driveFolderDialog.loading = false
                try {
                    driveFolderDialog.entries = JSON.parse(foldersJson)
                } catch (e) {
                    driveFolderDialog.entries = []
                }
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
            driveFolderPathField.text = ""
            localPathField.text = ""
        }

        Column {
            width: parent.width
            spacing: 6

            Label { text: qsTr("Display name") }
            TextField { id: displayNameField; width: parent.width }

            Label { text: qsTr("Drive folder id (leave empty for \"My Drive\" root)") }
            Row {
                width: parent.width
                spacing: 6
                TextField {
                    id: driveFolderIdField
                    width: parent.width - 90
                    // Manual edits invalidate the last picked path preview
                    // (the dialog re-sets it right after this, for picks).
                    onTextChanged: driveFolderPathField.text = ""
                }
                Button { text: qsTr("Browse\u2026"); width: 84; onClicked: driveFolderDialog.open() }
            }
            Label {
                id: driveFolderPathField
                width: parent.width
                opacity: 0.7
                elide: Text.ElideMiddle
                // Set by the Drive folder browser when a folder is picked;
                // shows a human-readable path since the field above holds
                // the (unreadable) Drive folder id actually sent to gdrived.
                visible: text.length > 0
                text: ""
            }

            Label { text: qsTr("Local path") }
            Row {
                width: parent.width
                spacing: 6
                TextField {
                    id: localPathField
                    width: parent.width - 90
                    placeholderText: qsTr("/home/user/GoogleDrive/Folder")
                }
                Button {
                    text: qsTr("Browse\u2026")
                    width: 84
                    onClicked: {
                        // Prefer a genuine native (KDE) folder picker via
                        // `kdialog` when available; fall back to our own
                        // FolderListModel-based browser otherwise.
                        if (syncManager.nativeFolderPickerAvailable()) {
                            var path = syncManager.pickLocalFolderNative(localPathField.text)
                            if (path.length > 0) {
                                localPathField.text = path
                            }
                        } else {
                            localFolderDialog.open()
                        }
                    }
                }
            }

            Label { text: qsTr("Owner user (optional)") }
            TextField { id: ownerUserField; width: parent.width }

            Label { text: qsTr("Owner group (optional)") }
            TextField { id: ownerGroupField; width: parent.width }
        }
    }
}

