// lgx-launcher — entry point of the "LGX Server.app" bundle.
//
// It checks the two TCC permissions once, shows a window naming exactly what is
// missing (and how to grant it), and only then starts the server
// (`lgx-server-bin`) as a child process. The launcher is the bundle's main
// executable on purpose: macOS attaches TCC grants to the app, and the server,
// capture and input helpers inherit them as children.
//
// The check is event driven, not a poll loop: it runs once at launch, on every
// button press, and when the app becomes active again (the user coming back
// from System Settings). Granting access makes the launcher exit so launchd
// starts it again: Screen Recording is only applied to a fresh process.
//
// While a session is streaming, the launcher holds a "no idle display sleep"
// assertion and releases it when the client disconnects, so an idle Mac can
// still sleep. LGX_NO_KEEP_AWAKE=1 disables that.
import AppKit
import ApplicationServices
import CoreGraphics
import IOKit.pwr_mgt

let bundleDir = URL(fileURLWithPath: CommandLine.arguments[0])
    .resolvingSymlinksInPath()
    .deletingLastPathComponent()
let serverBin = bundleDir.appendingPathComponent("lgx-server-bin")

func log(_ s: String) {
    FileHandle.standardError.write((s + "\n").data(using: .utf8)!)
}

func screenGranted() -> Bool { CGPreflightScreenCaptureAccess() }
func accessibilityGranted() -> Bool { AXIsProcessTrusted() }

// MARK: - Keeping the display awake while a client streams

var sleepAssertion: IOPMAssertionID = 0
var displayAwake = false

func setDisplayAwake(_ on: Bool) {
    guard on != displayAwake else { return }
    if on {
        var id: IOPMAssertionID = 0
        let status = IOPMAssertionCreateWithName(
            kIOPMAssertionTypePreventUserIdleDisplaySleep as CFString,
            IOPMAssertionLevel(kIOPMAssertionLevelOn),
            "LGX Server is streaming" as CFString,
            &id)
        if status == kIOReturnSuccess {
            sleepAssertion = id
            displayAwake = true
            log("lgx-launcher: display sleep blocked for the session")
        } else {
            log("lgx-launcher: cannot block display sleep (\(status))")
        }
    } else {
        IOPMAssertionRelease(sleepAssertion)
        displayAwake = false
        log("lgx-launcher: display sleep allowed again")
    }
}

// MARK: - Starting the server

func runServer() -> Never {
    guard FileManager.default.isExecutableFile(atPath: serverBin.path) else {
        log("lgx-launcher: missing \(serverBin.path)")
        exit(2)
    }

    let process = Process()
    process.executableURL = serverBin
    process.arguments = Array(CommandLine.arguments.dropFirst())
    // The server's stdout and stderr both end up in the LaunchAgent log, and
    // the launcher has to see the lines to toggle the sleep assertion.
    let pipe = Pipe()
    process.standardOutput = pipe
    process.standardError = pipe
    let keepAwake = ProcessInfo.processInfo.environment["LGX_NO_KEEP_AWAKE"] != "1"
    var pending = ""

    pipe.fileHandleForReading.readabilityHandler = { handle in
        let data = handle.availableData
        if data.isEmpty { return }
        FileHandle.standardError.write(data)
        guard keepAwake, let text = String(data: data, encoding: .utf8) else { return }
        pending += text
        while let newline = pending.firstIndex(of: "\n") {
            let line = String(pending[..<newline])
            pending = String(pending[pending.index(after: newline)...])
            if line.contains("authenticated") {
                setDisplayAwake(true)
            } else if line.contains("disconnected") {
                setDisplayAwake(false)
            }
        }
        if pending.count > 4096 { pending = String(pending.suffix(1024)) }
    }

    do {
        try process.run()
    } catch {
        log("lgx-launcher: cannot start server: \(error.localizedDescription)")
        exit(1)
    }

    // Forward termination to the server: a launchd bootout only signals the
    // job's main process, and an orphaned server keeps port 5902 (and the
    // capture) open.
    let signalQueue = DispatchQueue(label: "lgx.launcher.signals")
    var signalSources: [DispatchSourceSignal] = []
    for signalNumber in [SIGTERM, SIGINT] {
        signal(signalNumber, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: signalNumber, queue: signalQueue)
        source.setEventHandler {
            setDisplayAwake(false)
            process.terminate()
            process.waitUntilExit()
            exit(0)
        }
        source.resume()
        signalSources.append(source)
    }

    process.waitUntilExit()
    setDisplayAwake(false)
    exit(process.terminationStatus)
}

// MARK: - Permission window

func openSettings(_ anchor: String) {
    if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(anchor)") {
        NSWorkspace.shared.open(url)
    }
}

final class Permissions: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var statusLabel: NSTextField?
    private var buttons: [NSButton] = []

    func run() -> Never {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        app.delegate = self
        show()
        app.activate(ignoringOtherApps: true)
        app.run()
        exit(0)
    }

    private func show() {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 260),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false)
        window.title = "LGX Server — permissions"
        window.isReleasedWhenClosed = false
        window.center()

        let status = NSTextField(wrappingLabelWithString: "")
        status.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        let explain = NSTextField(wrappingLabelWithString:
            "Screen Recording and Accessibility are required before the server can start. "
            + "System Audio Recording is requested by macOS when the first audio client connects.")
        explain.font = NSFont.systemFont(ofSize: 12)
        explain.textColor = .secondaryLabelColor

        let stack = NSStackView(views: [status, explain])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false

        let screenButton = NSButton(title: "Open Screen Recording", target: self,
                                    action: #selector(openScreen))
        let axisButton = NSButton(title: "Open Accessibility", target: self,
                                  action: #selector(openAccessibility))
        let recheck = NSButton(title: "Recheck", target: self, action: #selector(recheck))
        recheck.keyEquivalent = "\r"
        let quit = NSButton(title: "Quit", target: self, action: #selector(quit))
        let row = NSStackView(views: [screenButton, axisButton, recheck, quit])
        row.orientation = .horizontal
        row.spacing = 8
        row.translatesAutoresizingMaskIntoConstraints = false

        let root = NSView()
        root.addSubview(stack)
        root.addSubview(row)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: root.topAnchor, constant: 20),
            row.leadingAnchor.constraint(equalTo: stack.leadingAnchor),
            row.topAnchor.constraint(greaterThanOrEqualTo: stack.bottomAnchor, constant: 16),
            row.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -16),
        ])
        window.contentView = root

        self.window = window
        self.statusLabel = status
        self.buttons = [screenButton, axisButton]
        refresh()
        window.makeKeyAndOrderFront(nil)
    }

    @objc private func refresh() {
        let screen = screenGranted()
        let axis = accessibilityGranted()
        let mark = { (ok: Bool) in ok ? "granted" : "missing" }
        statusLabel?.stringValue = [
            "Screen Recording   \(mark(screen))   — capture the desktop",
            "Accessibility      \(mark(axis))   — forward keyboard and mouse",
            "System Audio       asked on the first audio connection (optional)",
        ].joined(separator: "\n")
        buttons[0].isEnabled = !screen
        buttons[1].isEnabled = !axis
        log("lgx-launcher: Screen Recording=\(screen ? "ok" : "missing"), Accessibility=\(axis ? "ok" : "missing")")
        if screen && axis {
            // A fresh process is what makes the Screen Recording grant take
            // effect; launchd starts the launcher again immediately.
            log("lgx-launcher: permissions granted, restarting")
            exit(0)
        }
    }

    @objc private func recheck() { refresh() }

    @objc private func openScreen() {
        // Triggers the system prompt so LGX Server appears in the list.
        _ = CGRequestScreenCaptureAccess()
        openSettings("Privacy_ScreenCapture")
    }

    @objc private func openAccessibility() {
        let key = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
        _ = AXIsProcessTrustedWithOptions([key: true] as CFDictionary)
        openSettings("Privacy_Accessibility")
    }

    @objc private func quit() { exit(0) }

    func applicationDidBecomeActive(_ notification: Notification) {
        // One check when the user comes back from System Settings; no polling.
        refresh()
    }
}

// MARK: - Entry point

if screenGranted() && accessibilityGranted() {
    log("lgx-launcher: Screen Recording=ok, Accessibility=ok")
    runServer()
} else {
    Permissions().run()
}
