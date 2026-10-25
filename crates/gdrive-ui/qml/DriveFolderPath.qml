import QtQuick
import QtQuick.Controls
import QtQuick.Window

Label {
    id: control
    property string folderId: ""
    property string folderPath: ""

    text: !folderId || folderId === "root" ? qsTr("My Drive")
        : (folderPath || qsTr("Drive path unavailable; reselect folder"))
    textFormat: Text.PlainText
    elide: Text.ElideMiddle
    Accessible.name: text

    HoverHandler { id: hover }
    ToolTip {
        objectName: "driveFolderPathToolTip"
        visible: hover.hovered
        delay: 500
        margins: 12
        width: Math.min(implicitWidth, Math.max(1,
            (control.Window.window ? control.Window.window.width : control.width) - 2 * margins))
        text: control.text
        contentItem: Label {
            text: control.text
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: palette.toolTipText
        }
    }
}
