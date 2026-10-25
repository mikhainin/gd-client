import QtQuick
import QtQuick.Controls
import QtTest
import "../../qml"

TestCase {
    name: "SyncFolderSwitch"
    width: 200
    height: 100
    visible: true
    when: windowShown

    QtObject {
        id: backend
        property bool checked: false
    }

    SyncFolderSwitch {
        id: control
        backendChecked: backend.checked
    }

    SignalSpy {
        id: requests
        target: control
        signalName: "changeRequested"
    }

    function init() {
        backend.checked = false
        control.enabled = true
        requests.clear()
    }

    function test_accessible_request_preserves_backend_binding_after_failure() {
        control.Accessible.pressAction()
        compare(requests.count, 1)
        compare(requests.signalArguments[0][0], true)
        compare(control.checked, false)
        backend.checked = true
        compare(control.checked, true)
        control.Accessible.toggleAction()
        compare(requests.signalArguments[1][0], false)
        compare(control.checked, true)
        backend.checked = false
        compare(control.checked, false)
    }

    function test_pointer_request_waits_for_backend_and_retains_binding() {
        mouseClick(control)
        compare(requests.count, 1)
        compare(requests.signalArguments[0][0], true)
        compare(control.checked, false)
        backend.checked = true
        compare(control.checked, true)
        mouseClick(control)
        compare(requests.count, 2)
        compare(requests.signalArguments[1][0], false)
        compare(control.checked, true)
    }

    function test_disabled_accessibility_cannot_request_change() {
        control.enabled = false
        control.Accessible.pressAction()
        control.Accessible.toggleAction()
        compare(requests.count, 0)
        compare(control.checked, false)
    }
}
