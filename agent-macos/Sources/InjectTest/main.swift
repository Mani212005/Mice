import AppKit
import ApplicationServices
import CoreGraphics
import Foundation
import MiceMacSupport

@main
struct InjectTest {
    static func main() {
        var arguments = Array(CommandLine.arguments.dropFirst())
        var delay: TimeInterval = 3

        if arguments.first == "--delay" {
            guard arguments.count >= 3,
                  let requestedDelay = TimeInterval(arguments[1]),
                  requestedDelay >= 0 else {
                fputs("Usage: inject-test [--delay <seconds>] <text>\n", stderr)
                exit(64)
            }
            delay = requestedDelay
            arguments.removeFirst(2)
        }

        let text = arguments.joined(separator: " ")
        guard !text.isEmpty else {
            fputs("Usage: inject-test [--delay <seconds>] <text>\n", stderr)
            exit(64)
        }

        if delay > 0 {
            print("Focus the target editable field within \(delay.formatted()) seconds; injection will begin after the delay.")
            Thread.sleep(forTimeInterval: delay)
        }

        printDiagnostics()

        do {
            try AXSupport.inject(text)
            print("Set and verified the focused element's value through the Accessibility API.")
        } catch {
            do {
                let target = try AXSupport.focusedTarget()
                let targetPID = try AXSupport.processID(of: target.application)
                try typeWithKeyboard(text, targetPID: targetPID)
                let received = target.element.flatMap {
                    AXSupport.value($0, kAXValueAttribute)
                }?.contains(text) == true
                if received {
                    print("Posted and verified text through the CGEvent fallback to process \(targetPID).")
                } else {
                    print("Posted text through the CGEvent fallback to process \(targetPID), but the target did not expose it for accessibility read-back; confirm it visually.")
                }
            } catch {
                fputs("inject-test: \(error.localizedDescription)\n", stderr)
                exit(1)
            }
        }
    }

    static func typeWithKeyboard(_ text: String, targetPID: pid_t) throws {
        guard let source = CGEventSource(stateID: .combinedSessionState),
              let down = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: true),
              let up = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: false) else {
            throw AXError("Could not create keyboard events.")
        }
        for scalar in text.unicodeScalars {
            let units = Array(String(scalar).utf16)
            units.withUnsafeBufferPointer { buffer in
                down.keyboardSetUnicodeString(stringLength: buffer.count, unicodeString: buffer.baseAddress)
                up.keyboardSetUnicodeString(stringLength: buffer.count, unicodeString: buffer.baseAddress)
            }
            down.postToPid(targetPID)
            Thread.sleep(forTimeInterval: 0.01)
            up.postToPid(targetPID)
            Thread.sleep(forTimeInterval: 0.01)
        }
    }

    private static func printDiagnostics() {
        print("diagnostic AXIsProcessTrusted(): \(AXIsProcessTrusted())")
        print("diagnostic running on main thread: \(Thread.isMainThread)")
        let bundleIdentifier = NSWorkspace.shared.frontmostApplication?.bundleIdentifier ?? "<none>"
        print("diagnostic frontmost bundle identifier: \(bundleIdentifier)")

        let system = AXUIElementCreateSystemWide()
        var focused: CFTypeRef?
        let focusStatus = AXUIElementCopyAttributeValue(
            system,
            kAXFocusedUIElementAttribute as CFString,
            &focused
        )
        guard focusStatus == .success,
              let focused,
              CFGetTypeID(focused) == AXUIElementGetTypeID() else {
            print("diagnostic kAXFocusedUIElementAttribute: no AXUIElement (AXError: \(describe(focusStatus)))")
            printFocusedApplicationDiagnostic(system)
            return
        }

        let element = unsafeDowncast(focused, to: AXUIElement.self)
        print("diagnostic kAXFocusedUIElementAttribute: returned AXUIElement")
        print("diagnostic AXRole: \(stringAttribute(kAXRoleAttribute, on: element))")
        print("diagnostic AXSubrole: \(stringAttribute(kAXSubroleAttribute, on: element))")
        print("diagnostic AXValue supported: \(supportsAttribute(kAXValueAttribute, on: element))")
        print("diagnostic AXSelectedTextRange supported: \(supportsAttribute(kAXSelectedTextRangeAttribute, on: element))")
    }

    private static func printFocusedApplicationDiagnostic(_ system: AXUIElement) {
        var application: CFTypeRef?
        let applicationStatus = AXUIElementCopyAttributeValue(
            system,
            kAXFocusedApplicationAttribute as CFString,
            &application
        )
        guard applicationStatus == .success,
              let application,
              CFGetTypeID(application) == AXUIElementGetTypeID() else {
            print("diagnostic kAXFocusedApplicationAttribute: no AXUIElement (AXError: \(describe(applicationStatus)))")
            return
        }

        let applicationElement = unsafeDowncast(application, to: AXUIElement.self)
        var focused: CFTypeRef?
        let focusStatus = AXUIElementCopyAttributeValue(
            applicationElement,
            kAXFocusedUIElementAttribute as CFString,
            &focused
        )
        let returnedElement = focusStatus == .success
            && focused.map { CFGetTypeID($0) == AXUIElementGetTypeID() } == true
        let result = returnedElement ? "returned AXUIElement" : "no AXUIElement"
        print("diagnostic focused application AXFocusedUIElement: \(result) (AXError: \(describe(focusStatus)))")
    }

    private static func stringAttribute(_ attribute: String, on element: AXUIElement) -> String {
        var value: CFTypeRef?
        let status = AXUIElementCopyAttributeValue(element, attribute as CFString, &value)
        guard status == .success else { return "<unavailable: \(describe(status))>" }
        return (value as? String) ?? "<non-string value>"
    }

    private static func supportsAttribute(_ attribute: String, on element: AXUIElement) -> String {
        var names: CFArray?
        let status = AXUIElementCopyAttributeNames(element, &names)
        guard status == .success, let names else { return "unknown (\(describe(status)))" }
        let supported = (names as? [String])?.contains(attribute) ?? false
        return supported ? "yes" : "no"
    }

    private static func describe(_ error: ApplicationServices.AXError) -> String {
        let name: String
        switch error {
        case .success:
            name = "kAXErrorSuccess"
        case .cannotComplete:
            name = "kAXErrorCannotComplete"
        case .attributeUnsupported:
            name = "kAXErrorAttributeUnsupported"
        case .noValue:
            name = "kAXErrorNoValue"
        default:
            name = "AXError"
        }
        return "\(name) (raw value \(error.rawValue))"
    }
}
