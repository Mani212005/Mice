import AppKit
import ApplicationServices
import CoreGraphics

public struct AXElementDescription: Sendable {
    public let role: String?
    public let title: String?
    public let description: String?
    public let value: String?
    public let help: String?
    public let actions: [String]
}

public struct AXFocusedTarget {
    public let application: AXUIElement
    public let element: AXUIElement?
}

public enum AXSupport {
    public static func elementAtCursor() throws -> AXUIElement {
        let point = CGEvent(source: nil)?.location ?? .zero
        return try element(at: point)
    }

    public static func element(at point: CGPoint) throws -> AXUIElement {
        try MicePermission.accessibility.require()
        let system = AXUIElementCreateSystemWide()
        var element: AXUIElement?
        let status = AXUIElementCopyElementAtPosition(system, Float(point.x), Float(point.y), &element)
        guard status == .success, let element else {
            throw AXError("No accessibility element was found under the cursor (AX status \(status.rawValue)).")
        }
        return semanticElement(under: element, at: point) ?? element
    }

    public static func describe(_ element: AXUIElement) -> AXElementDescription {
        AXElementDescription(
            role: value(element, kAXRoleAttribute),
            title: value(element, kAXTitleAttribute),
            description: value(element, kAXDescriptionAttribute),
            value: value(element, kAXValueAttribute),
            help: value(element, kAXHelpAttribute),
            actions: actions(element)
        )
    }

    /// Produces a human-facing label without treating AXHelp/tooltips as the
    /// primary identity. Modern web UIs often put the visible label on a child
    /// or a nearby ancestor rather than the hit-tested container.
    public static func semanticText(_ element: AXUIElement) -> String? {
        if let label = meaningfulLabel(describe(element)) { return label }
        if let label = descendantLabel(of: element, inspectedNodes: 0) { return label }
        if let parent = copyElementAttribute(kAXParentAttribute, from: element).element,
           let label = meaningfulLabel(describe(parent)) { return label }
        return nil
    }

    public static func inject(_ text: String) throws {
        let element = try focusedElement()
        var settable = DarwinBoolean(false)
        guard AXUIElementIsAttributeSettable(element, kAXValueAttribute as CFString, &settable) == .success,
              settable.boolValue else {
            throw AXError("Focused element does not expose a settable accessibility value.")
        }
        guard AXUIElementSetAttributeValue(element, kAXValueAttribute as CFString, text as CFString) == .success else {
            throw AXError("Focused element rejected the accessibility value injection.")
        }
        guard value(element, kAXValueAttribute) == text else {
            throw AXError("Focused element did not return the injected value after the accessibility write.")
        }
    }

    public static func focusedElement() throws -> AXUIElement {
        guard let element = try focusedTarget().element else {
            throw AXError("The focused application did not expose a focused accessibility element.")
        }
        return element
    }

    /// Locates the target application even when its focused text element is not exposed.
    /// This permits keyboard-event fallback to remain available for partially accessible apps.
    public static func focusedTarget() throws -> AXFocusedTarget {
        try MicePermission.accessibility.require()

        // Apple's API documents the system-wide element as the object to use for
        // system attributes such as the focused object. Increase the per-process
        // messaging timeout before querying it because kAXErrorCannotComplete can
        // be a transient timeout/messaging failure.
        let system = AXUIElementCreateSystemWide()
        _ = AXUIElementSetMessagingTimeout(system, 2)

        let systemFocus = copyElementAttribute(kAXFocusedUIElementAttribute, from: system)
        if let element = systemFocus.element {
            let pid = try processID(of: element)
            return AXFocusedTarget(application: AXUIElementCreateApplication(pid), element: element)
        }

        let reportedApplication = copyElementAttribute(kAXFocusedApplicationAttribute, from: system)
        let application: AXUIElement
        if let reportedApplication = reportedApplication.element {
            application = reportedApplication
        } else if let frontmostApplication = NSWorkspace.shared.frontmostApplication {
            application = AXUIElementCreateApplication(frontmostApplication.processIdentifier)
        } else {
            throw AXError(
                "Could not obtain a focused accessibility application "
                    + "(system focus \(describe(systemFocus.status)); "
                    + "focused application \(describe(reportedApplication.status)))."
            )
        }

        let applicationFocus = copyElementAttribute(kAXFocusedUIElementAttribute, from: application)
        if let element = applicationFocus.element {
            return AXFocusedTarget(application: application, element: element)
        }

        var inspectedNodes = 0
        let focusedWindow = copyElementAttribute(kAXFocusedWindowAttribute, from: application)
        if let window = focusedWindow.element,
           let element = focusedDescendant(of: window, depth: 0, inspectedNodes: &inspectedNodes) {
            return AXFocusedTarget(application: application, element: element)
        }
        if let element = focusedDescendant(of: application, depth: 0, inspectedNodes: &inspectedNodes) {
            return AXFocusedTarget(application: application, element: element)
        }

        // AX clients are allowed to omit focused-element information. Returning
        // the application still lets the caller use the verified CGEvent fallback.
        return AXFocusedTarget(application: application, element: nil)
    }

    public static func processID(of element: AXUIElement) throws -> pid_t {
        var pid: pid_t = 0
        guard AXUIElementGetPid(element, &pid) == .success, pid > 0 else {
            throw AXError("Could not determine the focused element's application process.")
        }
        return pid
    }

    public static func value(_ element: AXUIElement, _ attribute: String) -> String? {
        var result: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &result) == .success,
              let result else { return nil }
        return result as? String
    }

    private static func copyElementAttribute(
        _ attribute: String,
        from element: AXUIElement
    ) -> (element: AXUIElement?, status: ApplicationServices.AXError) {
        var result: CFTypeRef?
        let status = AXUIElementCopyAttributeValue(element, attribute as CFString, &result)
        guard status == .success,
              let result,
              CFGetTypeID(result) == AXUIElementGetTypeID() else {
            return (nil, status)
        }
        return (unsafeDowncast(result, to: AXUIElement.self), status)
    }

    private static func describe(_ status: ApplicationServices.AXError) -> String {
        let name: String
        switch status {
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
        return "\(name) (\(status.rawValue))"
    }

    private static func focusedDescendant(
        of element: AXUIElement,
        depth: Int,
        inspectedNodes: inout Int
    ) -> AXUIElement? {
        guard depth <= 64, inspectedNodes < 2_048 else { return nil }
        inspectedNodes += 1

        if isFocused(element) {
            return element
        }

        var childrenValue: CFTypeRef?
        guard AXUIElementCopyAttributeValue(
            element,
            kAXChildrenAttribute as CFString,
            &childrenValue
        ) == .success,
        let children = childrenValue as? [Any] else {
            return nil
        }

        for child in children {
            guard let childObject = child as AnyObject?,
                  CFGetTypeID(childObject) == AXUIElementGetTypeID() else {
                continue
            }
            let childElement = unsafeDowncast(childObject, to: AXUIElement.self)
            if let focused = focusedDescendant(
                of: childElement,
                depth: depth + 1,
                inspectedNodes: &inspectedNodes
            ) {
                return focused
            }
        }
        return nil
    }

    private static func isFocused(_ element: AXUIElement) -> Bool {
        var focused: CFTypeRef?
        guard AXUIElementCopyAttributeValue(
            element,
            kAXFocusedAttribute as CFString,
            &focused
        ) == .success else {
            return false
        }
        return (focused as? Bool) == true
    }

    private static func actions(_ element: AXUIElement) -> [String] {
        var result: CFArray?
        guard AXUIElementCopyActionNames(element, &result) == .success,
              let result else { return [] }
        return result as? [String] ?? []
    }

    /// AX hit-testing may stop at an AXGroup even when its subtree contains the
    /// actual button/link under the pointer. Keep an already actionable hit,
    /// otherwise prefer a labelled actionable descendant that contains the
    /// pointer over generic layout containers.
    private static func semanticElement(under element: AXUIElement, at point: CGPoint) -> AXUIElement? {
        if semanticScore(element, at: point) >= 100 { return element }
        var inspectedNodes = 0
        return bestSemanticDescendant(of: element, at: point, inspectedNodes: &inspectedNodes)?.element
    }

    private static func bestSemanticDescendant(
        of element: AXUIElement,
        at point: CGPoint,
        inspectedNodes: inout Int
    ) -> (element: AXUIElement, score: Int)? {
        guard inspectedNodes < 256 else { return nil }
        inspectedNodes += 1
        var best: (element: AXUIElement, score: Int)?
        let score = semanticScore(element, at: point)
        if score > 0 { best = (element, score) }
        for child in children(of: element) {
            if let candidate = bestSemanticDescendant(of: child, at: point, inspectedNodes: &inspectedNodes),
               candidate.score > (best?.score ?? Int.min) {
                best = candidate
            }
        }
        return best
    }

    private static func semanticScore(_ element: AXUIElement, at point: CGPoint) -> Int {
        let role = value(element, kAXRoleAttribute) ?? ""
        let actionableRoles: Set<String> = [
            "AXButton", "AXLink", "AXTextField", "AXTextArea", "AXSearchField",
            "AXCheckBox", "AXRadioButton", "AXComboBox", "AXPopUpButton", "AXMenuButton",
        ]
        var score = actionableRoles.contains(role) ? 100 : 0
        if let title = value(element, kAXTitleAttribute), !title.isEmpty { score += 20 }
        if let description = value(element, kAXDescriptionAttribute), !description.isEmpty { score += 20 }
        if !actions(element).isEmpty { score += 10 }
        if role == kAXGroupRole || role == kAXLayoutAreaRole || role.isEmpty { score -= 30 }
        if let bounds = bounds(of: element), bounds.contains(point) { score += 15 }
        return score
    }

    private static func children(of element: AXUIElement) -> [AXUIElement] {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXChildrenAttribute as CFString, &value) == .success,
              let children = value as? [Any] else { return [] }
        return children.compactMap { child in
            guard let object = child as AnyObject?, CFGetTypeID(object) == AXUIElementGetTypeID() else { return nil }
            return unsafeDowncast(object, to: AXUIElement.self)
        }
    }

    private static func bounds(of element: AXUIElement) -> CGRect? {
        var positionValue: CFTypeRef?
        var sizeValue: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXPositionAttribute as CFString, &positionValue) == .success,
              AXUIElementCopyAttributeValue(element, kAXSizeAttribute as CFString, &sizeValue) == .success,
              let positionValue, let sizeValue,
              CFGetTypeID(positionValue) == AXValueGetTypeID(),
              CFGetTypeID(sizeValue) == AXValueGetTypeID() else { return nil }
        var position = CGPoint.zero
        var size = CGSize.zero
        guard AXValueGetValue(unsafeDowncast(positionValue, to: AXValue.self), .cgPoint, &position),
              AXValueGetValue(unsafeDowncast(sizeValue, to: AXValue.self), .cgSize, &size) else { return nil }
        return CGRect(origin: position, size: size)
    }

    private static func descendantLabel(of element: AXUIElement, inspectedNodes: Int) -> String? {
        guard inspectedNodes < 64 else { return nil }
        for child in children(of: element) {
            if let label = meaningfulLabel(describe(child)) { return label }
            if let label = descendantLabel(of: child, inspectedNodes: inspectedNodes + 1) { return label }
        }
        return nil
    }

    private static func meaningfulLabel(_ description: AXElementDescription) -> String? {
        [description.title, description.description, description.value]
            .compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines) }
            .first(where: { !isGenericLabel($0) })
    }

    private static func isGenericLabel(_ label: String) -> Bool {
        ["button", "group", "link", "text", "control", "status", "unknown"]
            .contains(label.lowercased())
    }
}

public struct AXError: LocalizedError {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var errorDescription: String? { message }
}
