import QtQuick
import QtQuick.Controls

Column {
    id: control
    property string folderId: ""
    property string folderPath: ""
    signal browseRequested()
    spacing: 6

    onFolderIdChanged: folderPath = ""

    function selectFolder(id, path) {
        folderId = id
        folderPath = path
        advanced.checked = false
    }

    function reset() {
        folderId = ""
        folderPath = ""
        advanced.checked = false
    }

    Label { text: qsTr("Drive folder") }
    Row {
        width: parent.width
        spacing: 6
        DriveFolderPath {
            objectName: "driveFolderPathField"
            width: parent.width - 90
            anchors.verticalCenter: parent.verticalCenter
            folderId: control.folderId
            folderPath: control.folderPath
        }
        AccessibleButton {
            objectName: "browseDriveFoldersButton"
            text: qsTr("Browse\u2026")
            Accessible.name: qsTr("Browse Drive folders")
            width: 84
            onClicked: control.browseRequested()
        }
    }
    CheckBox {
        id: advanced
        objectName: "advancedDriveFolderToggle"
        text: qsTr("Advanced: enter Drive folder ID")
        Accessible.onPressAction: {
            if (enabled)
                toggle()
        }
    }
    Column {
        width: parent.width
        visible: advanced.checked
        spacing: 6
        Label {
            width: parent.width
            text: qsTr("Drive folder ID (empty for My Drive)")
            wrapMode: Text.WordWrap
        }
        TextField {
            objectName: "driveFolderIdField"
            width: parent.width
            Accessible.name: qsTr("Drive folder id")
            text: control.folderId
            onTextEdited: control.folderId = text
        }
    }
}
