import QtQml

QtObject {
    property bool connected: false
    property bool authenticated: false
    property string syncState: "starting"
    property string syncError: ""

    readonly property string state: !connected || !authenticated ? "disconnected"
        : syncState === "unavailable" || syncState === "starting" ? "unavailable" : syncState
    readonly property url source: Qt.resolvedUrl("../icons/"
        + (state === "idle" ? "org.gclient.gdrive-ui" : state === "unavailable" ? "disconnected" : state)
        + ".svg")
    readonly property string tooltip: qsTr("g-client") + " - " + (!connected
        ? (syncState === "starting" ? qsTr("Connecting to gdrived") : qsTr("Disconnected from gdrived"))
        : !authenticated ? qsTr("Signed out")
        : syncState === "starting" ? qsTr("Reading sync status")
        : state === "unavailable" ? qsTr("Sync status unavailable") + (syncError ? "\n" + syncError : "")
        : state === "error" ? qsTr("Sync error") + "\n" + syncError
        : state === "syncing" ? qsTr("Syncing")
        : qsTr("Idle"))
}
