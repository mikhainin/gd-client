import QtQuick
import QtQuick.Controls
import QtTest
import "../../qml"

TestCase {
    id: testCase
    name: "DriveFolderDialog"
    width: 720
    height: 480
    visible: true
    when: windowShown

    property var dialog
    readonly property var testBackend: backend
    property int heartbeats: 0

    Timer {
        interval: 10
        repeat: true
        running: true
        onTriggered: testCase.heartbeats++
    }

    QtObject {
        id: backend
        property string driveFoldersJson: "[]"
        property string driveFoldersError: ""
        property bool driveFoldersLoading: false
        property int generation: 0
        property int requests: 0
        property string parentId: ""

        function listDriveFolders(parent) {
            generation++
            requests++
            parentId = parent
            driveFoldersJson = "[]"
            driveFoldersError = ""
            driveFoldersLoading = true
        }

        function cancelDriveFolders() {
            generation++
            driveFoldersLoading = false
        }

        function complete(requestGeneration, rows, error) {
            if (requestGeneration !== generation)
                return
            driveFoldersJson = JSON.stringify(rows)
            driveFoldersError = error || ""
            driveFoldersLoading = false
        }
    }

    Component {
        id: dialogComponent
        DriveFolderDialog {
            backend: testCase.testBackend
            enter: Transition {}
            exit: Transition {}
        }
    }

    SignalSpy {
        id: selection
        signalName: "folderSelected"
    }

    SignalSpy {
        id: acceptance
        signalName: "accepted"
    }

    function init() {
        backend.driveFoldersJson = "[]"
        backend.driveFoldersError = ""
        backend.driveFoldersLoading = false
        backend.requests = 0
        dialog = createTemporaryObject(dialogComponent, testCase)
        verify(dialog !== null)
        selection.target = dialog
        selection.clear()
        acceptance.target = dialog
        acceptance.clear()
        dialog.open()
        tryCompare(dialog, "opened", true)
        compare(backend.requests, 1)
    }

    function cleanup() {
        if (dialog)
            dialog.close()
        selection.target = null
        acceptance.target = null
    }

    function test_loading_keeps_event_loop_and_cancel_responsive() {
        compare(dialog.loading, true)
        verify(findChild(dialog, "driveFoldersLoadingLabel").visible)
        verify(!findChild(dialog, "driveFoldersEmptyLabel").visible)
        verify(!findChild(dialog, "selectDriveFolderButton").enabled)
        var before = heartbeats
        wait(80)
        verify(heartbeats > before + 2, "GUI heartbeat stopped during pending request")
        var start = Date.now()
        mouseClick(dialog.standardButton(Dialog.Cancel))
        tryCompare(dialog, "visible", false, 500)
        verify(Date.now() - start < 500, "Cancel did not respond promptly")
        compare(backend.driveFoldersLoading, false)
    }

    function test_error_is_not_empty_and_retry_recovers() {
        backend.complete(backend.generation, [], "Drive request timed out")
        verify(findChild(dialog, "driveFoldersErrorLabel").visible)
        compare(findChild(dialog, "driveFoldersErrorLabel").text, "Drive request timed out")
        verify(!findChild(dialog, "driveFoldersEmptyLabel").visible)
        verify(!findChild(dialog, "selectDriveFolderButton").enabled)
        mouseClick(findChild(dialog, "retryDriveFoldersButton"))
        compare(backend.requests, 2)
        compare(dialog.loading, true)
        compare(dialog.errorMessage, "")
        backend.complete(backend.generation, [], "")
        verify(findChild(dialog, "driveFoldersEmptyLabel").visible)
        verify(findChild(dialog, "selectDriveFolderButton").enabled)
    }

    function test_accessible_press_respects_enabled_and_selects_once() {
        var select = findChild(dialog, "selectDriveFolderButton")
        select.Accessible.pressAction()
        compare(selection.count, 0)
        backend.complete(backend.generation, [], "")
        select.Accessible.pressAction()
        compare(selection.count, 1)
        compare(selection.signalArguments[0][0], "")
    }

    function test_accessible_cancel_during_loading() {
        dialog.standardButton(Dialog.Cancel).Accessible.pressAction()
        tryCompare(dialog, "visible", false, 500)
        compare(backend.driveFoldersLoading, false)
    }

    function test_standard_accept_emits_once_from_accessibility() {
        dialog.standardButtons = Dialog.Ok | Dialog.Cancel
        var button = dialog.standardButton(Dialog.Ok)
        verify(button !== null)
        button.Accessible.pressAction()
        compare(acceptance.count, 1)
        tryCompare(dialog, "visible", false)
    }

    function test_root_and_nested_selection_values() {
        backend.complete(backend.generation, [], "")
        mouseClick(findChild(dialog, "selectDriveFolderButton"))
        compare(selection.count, 1)
        compare(selection.signalArguments[0][0], "")
        compare(selection.signalArguments[0][1], "My Drive")
        dialog.open()
        tryCompare(dialog, "opened", true)
        backend.complete(backend.generation, [{id: "child", name: "Projects"}], "")
        var list = findChild(dialog, "driveFolderList")
        tryCompare(list, "count", 1)
        var delegate = list.itemAtIndex(0)
        verify(delegate !== null)
        verify(delegate.visible)
        verify(delegate.width > 0 && delegate.height > 0)
        waitForRendering(delegate)
        mouseClick(delegate)
        compare(backend.parentId, "child")
        compare(dialog.pathStack.length, 2)
        backend.complete(backend.generation, [], "")
        mouseClick(findChild(dialog, "selectDriveFolderButton"))
        compare(selection.count, 2)
        compare(selection.signalArguments[1][0], "child")
        compare(selection.signalArguments[1][1], "My Drive / Projects")
    }

    function test_breadcrumb_navigation_discards_superseded_reply() {
        var old = backend.generation
        dialog.enterFolder("child", "Projects")
        var childRequest = backend.generation
        dialog.goTo(0)
        compare(backend.parentId, "")
        backend.complete(old, [{id: "obsolete", name: "Old"}], "")
        backend.complete(childRequest, [], "obsolete error")
        compare(dialog.loading, true)
        compare(dialog.errorMessage, "")
        backend.complete(backend.generation, [], "")
        compare(dialog.entries.length, 0)
        compare(dialog.currentFolder().id, "")
    }

    function test_close_and_reopen_rejects_late_results() {
        var old = backend.generation
        dialog.close()
        tryCompare(dialog, "visible", false)
        dialog.open()
        tryCompare(dialog, "opened", true)
        backend.complete(old, [{id: "stale", name: "Stale"}], "")
        compare(dialog.loading, true)
        compare(dialog.entries.length, 0)
        backend.complete(backend.generation, [{id: "new", name: "New"}], "")
        compare(dialog.entries[0].id, "new")
    }
}
