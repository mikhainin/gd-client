import QtQuick
import QtTest
import "../../qml"

TestCase {
    name: "TrayPresentation"

    TrayPresentation {
        id: presentation
    }

    Image {
        id: decoded
        source: presentation.source
        sourceSize: Qt.size(22, 22)
    }

    function init() {
        presentation.connected = true
        presentation.authenticated = true
        presentation.syncState = "idle"
        presentation.syncError = ""
    }

    function test_connection_and_authentication_override_stale_status() {
        presentation.syncState = "error"
        presentation.syncError = "upload failed"
        presentation.connected = false
        compare(presentation.state, "disconnected")
        verify(presentation.tooltip.indexOf("Disconnected") >= 0)
        presentation.connected = true
        presentation.authenticated = false
        compare(presentation.state, "disconnected")
        verify(presentation.tooltip.indexOf("Signed out") >= 0)
        presentation.authenticated = true
        compare(presentation.state, "error")
        verify(presentation.tooltip.indexOf("upload failed") >= 0)
    }

    function test_startup_is_distinct_from_connection_failure() {
        presentation.connected = false
        presentation.syncState = "starting"
        verify(presentation.tooltip.indexOf("Connecting") >= 0)
        presentation.syncState = "unavailable"
        verify(presentation.tooltip.indexOf("Disconnected") >= 0)
        presentation.connected = true
        presentation.syncState = "starting"
        compare(presentation.state, "unavailable")
        verify(presentation.tooltip.indexOf("Reading sync status") >= 0)
    }

    function test_live_transitions_and_unavailable_are_not_idle() {
        const idle = presentation.source
        presentation.syncState = "syncing"
        compare(presentation.state, "syncing")
        verify(presentation.source !== idle)
        verify(presentation.tooltip.indexOf("Syncing") >= 0)
        presentation.syncState = "error"
        presentation.syncError = "initial sync failed"
        verify(presentation.tooltip.indexOf("initial sync failed") >= 0)
        presentation.syncState = "unavailable"
        presentation.syncError = "UnknownMethod"
        compare(presentation.state, "unavailable")
        verify(presentation.tooltip.indexOf("Sync status unavailable") >= 0)
        verify(presentation.tooltip.indexOf("UnknownMethod") >= 0)
        presentation.syncState = "idle"
        presentation.syncError = ""
        compare(presentation.source, idle)
        verify(presentation.tooltip.indexOf("Idle") >= 0)
    }

    function test_each_selected_svg_decodes() {
        for (const state of ["idle", "syncing", "error", "disconnected", "unavailable"]) {
            presentation.syncState = state
            tryCompare(decoded, "status", Image.Ready)
            compare(decoded.implicitWidth, 22)
            compare(decoded.implicitHeight, 22)
        }
    }
}
