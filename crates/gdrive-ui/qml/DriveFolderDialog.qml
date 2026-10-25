import QtQuick
import QtQuick.Controls

Dialog {
    id: dialog
    objectName: "driveFolderDialog"
    title: qsTr("Select Drive folder")
    modal: true
    standardButtons: Dialog.Cancel
    footer: DialogButtonBox {
        Accessible.role: Accessible.Grouping
        Accessible.name: dialog.title
        delegate: AccessibleButton {}
    }
    width: Math.min(420, parent.width - 40)
    height: Math.min(420, parent.height - 40)

    required property var backend
    property var pathStack: [{ id: "", name: qsTr("My Drive") }]
    readonly property var entries: JSON.parse(backend.driveFoldersJson)
    readonly property bool loading: backend.driveFoldersLoading
    readonly property string errorMessage: backend.driveFoldersError
    signal folderSelected(string folderId, string folderPath)

    function currentFolder() {
        return pathStack[pathStack.length - 1]
    }

    function load() {
        backend.listDriveFolders(currentFolder().id)
    }

    function enterFolder(id, name) {
        pathStack = pathStack.concat([{ id: id, name: name }])
        load()
    }

    function goTo(index) {
        pathStack = pathStack.slice(0, index + 1)
        load()
    }

    onAboutToShow: {
        pathStack = [{ id: "", name: qsTr("My Drive") }]
        load()
    }
    onAboutToHide: backend.cancelDriveFolders()

    Column {
        Accessible.role: Accessible.Dialog
        Accessible.name: dialog.title
        anchors.fill: parent
        spacing: 6

        Row {
            width: parent.width
            spacing: 4
            Repeater {
                model: dialog.pathStack
                delegate: AccessibleButton {
                    text: modelData.name
                    flat: true
                    Accessible.name: qsTr("Browse %1").arg(modelData.name)
                    onClicked: dialog.goTo(index)
                }
            }
        }

        Item {
            width: parent.width
            height: parent.height - 90

            ListView {
                id: folderList
                objectName: "driveFolderList"
                anchors.fill: parent
                clip: true
                visible: !dialog.loading && !dialog.errorMessage
                model: dialog.entries
                ScrollBar.vertical: ScrollBar { policy: ScrollBar.AlwaysOn; width: 14 }
                delegate: ItemDelegate {
                    width: ListView.view.width
                    text: modelData.name
                    Accessible.name: qsTr("Open folder %1").arg(modelData.name)
                    Accessible.onPressAction: {
                        if (enabled)
                            clicked()
                    }
                    onClicked: dialog.enterFolder(modelData.id, modelData.name)
                }
            }

            Column {
                anchors.centerIn: parent
                visible: dialog.loading
                BusyIndicator {
                    anchors.horizontalCenter: parent.horizontalCenter
                    running: dialog.loading
                }
                Label {
                    objectName: "driveFoldersLoadingLabel"
                    text: qsTr("Loading Drive folders...")
                }
            }

            Column {
                width: parent.width
                anchors.centerIn: parent
                spacing: 8
                visible: !dialog.loading && dialog.errorMessage.length > 0
                Label {
                    objectName: "driveFoldersErrorLabel"
                    width: parent.width
                    text: dialog.errorMessage
                    wrapMode: Text.WordWrap
                    Accessible.name: text
                }
                AccessibleButton {
                    objectName: "retryDriveFoldersButton"
                    text: qsTr("Retry")
                    onClicked: dialog.load()
                }
            }

            Label {
                objectName: "driveFoldersEmptyLabel"
                anchors.centerIn: parent
                visible: !dialog.loading && !dialog.errorMessage && dialog.entries.length === 0
                text: qsTr("No sub-folders here.")
                opacity: 0.6
            }
        }

        AccessibleButton {
            objectName: "selectDriveFolderButton"
            width: parent.width
            enabled: !dialog.loading && !dialog.errorMessage
            text: qsTr("Select \"%1\"").arg(dialog.currentFolder().name)
            Accessible.name: qsTr("Select this folder")
            Accessible.description: dialog.pathStack.map(entry => entry.name).join("/")
            onClicked: {
                dialog.folderSelected(
                    dialog.currentFolder().id,
                    dialog.pathStack.map(entry => entry.name).join("/"))
                dialog.close()
            }
        }
    }
}
