import QtQuick
import QtQuick.Controls

Button {
    id: control
    // Qt 6.4 advertises Press without forwarding it to the clicked signal.
    Accessible.onPressAction: {
        if (control.enabled)
            control.clicked()
    }
}
