import QtQuick
import QtQuick.Controls
import QtTest

TestCase {
    name: "SkillSemanticControls"
    width: 400
    height: 200
    visible: true
    when: windowShown

    Button {
        id: button
        text: "Fixture action"
        Accessible.name: text
        Accessible.onPressAction: {
            if (enabled)
                clicked()
        }
    }

    TextField {
        id: field
        y: 60
        width: 200
        Accessible.name: "Fixture text"
    }

    SignalSpy {
        id: clicks
        target: button
        signalName: "clicked"
    }

    function init() {
        button.enabled = true
        clicks.clear()
        field.clear()
    }

    function test_semantic_press_fires_once() {
        button.Accessible.pressAction()
        compare(clicks.count, 1)
    }

    function test_disabled_semantic_press_does_not_activate() {
        button.enabled = false
        button.Accessible.pressAction()
        compare(clicks.count, 0)
    }

    function test_native_text_input_without_editable_text_interface() {
        field.forceActiveFocus()
        verify(field.activeFocus)
        keyClick(Qt.Key_A)
        keyClick(Qt.Key_B)
        keyClick(Qt.Key_C)
        compare(field.text, "abc")
    }
}
