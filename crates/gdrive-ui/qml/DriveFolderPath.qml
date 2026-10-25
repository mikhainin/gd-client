import QtQuick
import QtQuick.Controls

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
        visible: hover.hovered
        delay: 500
        text: control.text
        contentItem: Label {
            text: control.text
            textFormat: Text.PlainText
            color: palette.toolTipText
        }
    }
}
