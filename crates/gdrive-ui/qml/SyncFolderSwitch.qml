import QtQuick
import QtQuick.Controls

Switch {
    id: control
    property bool backendChecked: false
    signal changeRequested(bool desired)
    checked: backendChecked

    function requestAccessibleToggle() {
        if (enabled)
            changeRequested(!backendChecked)
    }

    Accessible.onToggleAction: requestAccessibleToggle()
    Accessible.onPressAction: requestAccessibleToggle()
    onToggled: {
        var desired = checked
        checked = Qt.binding(function() { return control.backendChecked })
        changeRequested(desired)
    }
}
