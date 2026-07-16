@preconcurrency import ApplicationServices
import CoreGraphics

public enum MicePermission: String, CaseIterable {
    case screenRecording = "Screen Recording"
    case accessibility = "Accessibility"
    case inputMonitoring = "Input Monitoring"

    public var granted: Bool {
        switch self {
        case .screenRecording:
            return CGPreflightScreenCaptureAccess()
        case .accessibility:
            return AXIsProcessTrusted()
        case .inputMonitoring:
            return IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) == kIOHIDAccessTypeGranted
        }
    }

    public func request() {
        switch self {
        case .screenRecording:
            _ = CGRequestScreenCaptureAccess()
        case .accessibility:
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
                as CFDictionary
            _ = AXIsProcessTrustedWithOptions(options)
        case .inputMonitoring:
            _ = IOHIDRequestAccess(kIOHIDRequestTypeListenEvent)
        }
    }

    public func require() throws {
        guard granted else {
            request()
            throw MicePermissionError.missing(rawValue)
        }
    }
}

public enum MicePermissionError: LocalizedError {
    case missing(String)

    public var errorDescription: String? {
        switch self {
        case let .missing(permission):
            return "Missing \(permission) permission. Grant it to Terminal (or this executable) in System Settings > Privacy & Security, then retry."
        }
    }
}
