// Synthetic input for UI smoke tests (needs Accessibility permission for
// the calling terminal). Coordinates are global screen points.
//
//   ui-driver window <pid>                      -> x y w h id of the pid's largest window
//   ui-driver wait-window <pid> [timeout_ms]    -> like window, polling until it exists
//   ui-driver front <pid>                       activate the app (no Apple Events)
//   ui-driver type TEXT                         unicode keystrokes to the front app
//   ui-driver key CODE|CHAR [mods]              mods: cmd,shift,ctrl,alt (comma list)
//   ui-driver perms                             -> "accessibility=0/1 screen=0/1"
//   ui-driver drag x1 y1 x2 y2 [shift]
//   ui-driver click x y [count] [shift]
//   ui-driver rclick x y
//   ui-driver scroll x y lines [shift]          (positive = up; shift: shift+wheel as macOS delivers it,
//                                                  sideways, positive = left)
//   ui-driver swipe x y dx [steps]              phased trackpad pixel scroll (positive dx = right)
//   ui-driver pinch x y scale [steps]           trackpad magnify gesture (best effort: private
//                                                  CGEvent gesture fields; may not reach AppKit)
//   ui-driver move x y
//   ui-driver mean file.png                     -> mean brightness (0-255)
//   ui-driver down x y | dragto x y | up x y    split drag (hold the button between calls)
//   ui-driver crop in.png x y w h out.png       crop (image pixels); out.raw -> raw RGB bytes
//   ui-driver env <pid> NAME                    -> the value of NAME in the pid's environment
//                                                  (exit 1 if unset or unreadable)
import AppKit
import ApplicationServices
import CoreGraphics
import ImageIO
import Foundation

let args = CommandLine.arguments
func num(_ i: Int) -> Double { Double(args[i])! }
let source = CGEventSource(stateID: .hidSystemState)
// UI_PID set: deliver key events straight to that process instead of the
// HID tap (works where the session-wide tap is filtered).
let targetPid = ProcessInfo.processInfo.environment["UI_PID"].flatMap { Int32($0) }

func post(_ type: CGEventType, _ p: CGPoint, _ button: CGMouseButton = .left, flags: CGEventFlags = [], clicks: Int64 = 1) {
    let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p, mouseButton: button)!
    e.flags = flags
    e.setIntegerValueField(.mouseEventClickState, value: clicks)
    e.post(tap: .cghidEventTap)
}

func windowRect(_ pid: Int32) -> ([String: Double], Int)? {
    let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as! [[String: Any]]
    // The largest on-screen normal window of the process.
    var best: ([String: Double], Int)? = nil
    for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid && (w[kCGWindowLayer as String] as? Int) == 0
        && (w[kCGWindowIsOnscreen as String] as? Bool) == true {
        let b = w[kCGWindowBounds as String] as! [String: Double]
        let id = w[kCGWindowNumber as String] as? Int ?? 0
        if best == nil || b["Width"]! * b["Height"]! > best!.0["Width"]! * best!.0["Height"]! { best = (b, id) }
    }
    return best
}

func key(_ code: CGKeyCode, _ flags: CGEventFlags = [], text: String? = nil) {
    for down in [true, false] {
        let e = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)!
        e.flags = flags
        if let text {
            let units = Array(text.utf16)
            e.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units)
        }
        if let targetPid { e.postToPid(targetPid) } else { e.post(tap: .cghidEventTap) }
        usleep(4_000)
    }
}

// US layout: character -> (virtual key, needs shift). GPUI derives text
// from the key code, so a unicode string alone is not enough.
let usKeys: [Character: (CGKeyCode, Bool)] = {
    var m: [Character: (CGKeyCode, Bool)] = [:]
    let plain: [(String, CGKeyCode)] = [
        ("a", 0), ("s", 1), ("d", 2), ("f", 3), ("h", 4), ("g", 5), ("z", 6), ("x", 7), ("c", 8), ("v", 9),
        ("b", 11), ("q", 12), ("w", 13), ("e", 14), ("r", 15), ("y", 16), ("t", 17), ("1", 18), ("2", 19),
        ("3", 20), ("4", 21), ("6", 22), ("5", 23), ("=", 24), ("9", 25), ("7", 26), ("-", 27), ("8", 28),
        ("0", 29), ("]", 30), ("o", 31), ("u", 32), ("[", 33), ("i", 34), ("p", 35), ("l", 37), ("j", 38),
        ("'", 39), ("k", 40), (";", 41), ("\\", 42), (",", 43), ("/", 44), ("n", 45), ("m", 46), (".", 47),
        ("\t", 48), (" ", 49), ("`", 50), ("\n", 36),
    ]
    let shifted: [(String, CGKeyCode)] = [
        ("!", 18), ("@", 19), ("#", 20), ("$", 21), ("^", 22), ("%", 23), ("+", 24), ("(", 25), ("&", 26),
        ("_", 27), ("*", 28), (")", 29), ("}", 30), ("{", 33), ("\"", 39), (":", 41), ("|", 42), ("<", 43),
        ("?", 44), (">", 47), ("~", 50),
    ]
    for (s, c) in plain { m[Character(s)] = (c, false) }
    for (s, c) in shifted { m[Character(s)] = (c, true) }
    for (s, c) in plain where s.count == 1 && s.first!.isLetter { m[Character(s.uppercased())] = (c, true) }
    return m
}()

switch args[1] {
case "env":
    // KERN_PROCARGS2: `int32 argc | exec path NUL | NUL padding | argc
    // NUL-terminated arguments (any may be empty) | NUL-terminated
    // environment entries`, ended by an empty string or the buffer:
    // exact entries, unlike `ps eww`'s space-joined text.
    guard args.count > 3, let pid = Int32(args[2]) else { exit(1) }
    var mib: [Int32] = [CTL_KERN, KERN_PROCARGS2, pid]
    var size = 0
    guard sysctl(&mib, 3, nil, &size, nil, 0) == 0, size > 4 else { exit(1) }
    var buffer = [UInt8](repeating: 0, count: size)
    guard sysctl(&mib, 3, &buffer, &size, nil, 0) == 0 else { exit(1) }
    let argc = Int(buffer.withUnsafeBytes { $0.load(as: Int32.self) })
    var at = 4
    /// The NUL-terminated string at `at`, moving past it.
    func next() -> String? {
        guard at < size, let end = buffer[at..<size].firstIndex(of: 0) else { return nil }
        defer { at = end + 1 }
        return String(decoding: buffer[at..<end], as: UTF8.self)
    }
    guard next() != nil else { exit(1) }  // exec path
    while at < size && buffer[at] == 0 { at += 1 }  // padding
    for _ in 0..<argc { guard next() != nil else { exit(1) } }
    let prefix = args[3] + "="
    while let entry = next(), !entry.isEmpty {
        if entry.hasPrefix(prefix) {
            print(entry.dropFirst(prefix.count))
            exit(0)
        }
    }
    exit(1)
case "window", "wait-window":
    guard args.count > 2, let pid = Int32(args[2]) else { exit(1) }
    let deadline = Date().addingTimeInterval((args.count > 3 ? num(3) : 0) / 1000)
    var rect = windowRect(pid)
    while rect == nil && args[1] == "wait-window" && Date() < deadline {
        usleep(50_000)
        rect = windowRect(pid)
    }
    guard let (b, id) = rect else { exit(1) }
    print(Int(b["X"]!), Int(b["Y"]!), Int(b["Width"]!), Int(b["Height"]!), id)
    exit(0)
case "front":
    guard args.count > 2, let pid = Int32(args[2]), let app = NSRunningApplication(processIdentifier: pid) else { exit(1) }
    app.activate(options: [.activateAllWindows])
    // Wait until it is actually frontmost (bounded).
    for _ in 0..<40 where NSWorkspace.shared.frontmostApplication?.processIdentifier != pid { usleep(25_000) }
    exit(NSWorkspace.shared.frontmostApplication?.processIdentifier == pid ? 0 : 1)
case "type":
    for ch in args[2] {
        guard let (code, shift) = usKeys[ch] else { fputs("no key for \(ch)\n", stderr); exit(2) }
        key(code, shift ? .maskShift : [], text: String(ch))
    }
case "key":
    var flags: CGEventFlags = []
    for m in (args.count > 3 ? args[3] : "").split(separator: ",") {
        switch m {
        case "cmd": flags.insert(.maskCommand)
        case "shift": flags.insert(.maskShift)
        case "ctrl": flags.insert(.maskControl)
        case "alt": flags.insert(.maskAlternate)
        default: fputs("unknown modifier \(m)\n", stderr); exit(2)
        }
    }
    // One character is a US-layout key ("1" is the digit); a longer number
    // a virtual key code.
    if args[2].count == 1, let (code, shift) = usKeys[args[2].first!] {
        key(code, shift ? flags.union(.maskShift) : flags)
    } else if let code = CGKeyCode(args[2]) {
        key(code, flags)
    } else {
        fputs("unknown key \(args[2])\n", stderr); exit(2)
    }
case "perms":
    print("accessibility=\(AXIsProcessTrusted() ? 1 : 0) screen=\(CGPreflightScreenCaptureAccess() ? 1 : 0)")
    exit(0)
case "crop":
    guard let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[2]) as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(src, 0, nil),
          let part = image.cropping(to: CGRect(x: num(3), y: num(4), width: num(5), height: num(6))) else { exit(1) }
    let out = URL(fileURLWithPath: args[7])
    if out.pathExtension == "raw" {
        let (w, h) = (part.width, part.height)
        guard let context = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                      space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(1) }
        context.draw(part, in: CGRect(x: 0, y: 0, width: w, height: h))
        let bytes = context.data!.assumingMemoryBound(to: UInt8.self)
        var rgb = Data(capacity: w * h * 3)
        for i in 0..<(w * h) { rgb.append(contentsOf: [bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2]]) }
        try! rgb.write(to: out)
        print(w, h)
    } else {
        guard let dest = CGImageDestinationCreateWithURL(out as CFURL, "public.png" as CFString, 1, nil) else { exit(1) }
        CGImageDestinationAddImage(dest, part, nil)
        CGImageDestinationFinalize(dest)
    }
    exit(0)
case "down":
    let p = CGPoint(x: num(2), y: num(3))
    post(.mouseMoved, p)
    usleep(50_000)
    post(.leftMouseDown, p)
case "dragto":
    post(.leftMouseDragged, CGPoint(x: num(2), y: num(3)))
case "up":
    post(.leftMouseUp, CGPoint(x: num(2), y: num(3)))
case "move":
    post(.mouseMoved, CGPoint(x: num(2), y: num(3)))
case "mean":
    // Mean brightness (0-255) of an image file, e.g. a `screencapture -R`
    // of the cursor cell.
    guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[2]) as CFURL, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
          let context = CGContext(data: nil, width: image.width, height: image.height, bitsPerComponent: 8,
                                  bytesPerRow: image.width * 4, space: CGColorSpaceCreateDeviceRGB(),
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(1) }
    context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
    let bytes = context.data!.assumingMemoryBound(to: UInt8.self)
    var total = 0
    for i in 0..<(image.width * image.height) {
        total += (Int(bytes[i * 4]) + Int(bytes[i * 4 + 1]) + Int(bytes[i * 4 + 2])) / 3
    }
    print(total / max(image.width * image.height, 1))
    exit(0)
case "drag":
    let flags: CGEventFlags = args.count > 6 ? .maskShift : []
    let a = CGPoint(x: num(2), y: num(3)), b = CGPoint(x: num(4), y: num(5))
    post(.mouseMoved, a)
    usleep(50_000)
    post(.leftMouseDown, a, flags: flags)
    for i in 1...20 {
        let t = Double(i) / 20
        post(.leftMouseDragged, CGPoint(x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t), flags: flags)
        usleep(15_000)
    }
    post(.leftMouseUp, b, flags: flags)
case "click":
    let p = CGPoint(x: num(2), y: num(3))
    let count = args.count > 4 ? Int64(args[4])! : 1
    let flags: CGEventFlags = args.count > 5 ? .maskShift : []
    post(.mouseMoved, p)
    usleep(30_000)
    for n in 1...count {
        post(.leftMouseDown, p, flags: flags, clicks: n)
        usleep(20_000)
        post(.leftMouseUp, p, flags: flags, clicks: n)
        usleep(60_000)
    }
case "rclick":
    let p = CGPoint(x: num(2), y: num(3))
    post(.mouseMoved, p)
    usleep(30_000)
    post(.rightMouseDown, p, .right)
    usleep(20_000)
    post(.rightMouseUp, p, .right)
case "scroll":
    post(.mouseMoved, CGPoint(x: num(2), y: num(3)))
    usleep(30_000)
    let lines = Int32(args[4])!
    let shift = args.count > 5 && args[5] == "shift"
    for _ in 0..<abs(lines) {
        let step: Int32 = lines > 0 ? 1 : -1
        let e = CGEvent(scrollWheelEvent2Source: source, units: .line, wheelCount: 2, wheel1: shift ? 0 : step, wheel2: shift ? step : 0, wheel3: 0)!
        if shift { e.flags = .maskShift }
        e.post(tap: .cghidEventTap)
        usleep(10_000)
    }
case "swipe":
    // kCGScrollPhase: began 1, changed 2, ended 4.
    post(.mouseMoved, CGPoint(x: num(2), y: num(3)))
    usleep(30_000)
    let steps = args.count > 5 ? Int(args[5])! : 16
    let step = Int32(num(4) / Double(steps))
    func phased(_ dx: Int32, _ phase: Int64) {
        let e = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2, wheel1: 0, wheel2: dx, wheel3: 0)!
        e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        e.setIntegerValueField(.scrollWheelEventScrollPhase, value: phase)
        e.post(tap: .cghidEventTap)
        usleep(12_000)
    }
    phased(0, 1)
    for _ in 0..<steps { phased(step, 2) }
    phased(0, 4)
case "pinch":
    // An NSEventTypeGesture (29) carrying a zoom (HID type 8): fields 110
    // (HID type), 113 (magnification), 132 (phase: began 1, changed 2, ended 4).
    let p = CGPoint(x: num(2), y: num(3))
    post(.mouseMoved, p)
    usleep(30_000)
    let steps = args.count > 5 ? Int(args[5])! : 12
    let per = pow(num(4), 1.0 / Double(steps)) - 1
    func magnify(_ value: Double, _ phase: Int64) {
        guard let e = CGEvent(source: source) else { return }
        e.type = CGEventType(rawValue: 29)!
        e.location = p
        e.setIntegerValueField(CGEventField(rawValue: 110)!, value: 8)
        e.setDoubleValueField(CGEventField(rawValue: 113)!, value: value)
        e.setIntegerValueField(CGEventField(rawValue: 132)!, value: phase)
        e.post(tap: .cghidEventTap)
        usleep(16_000)
    }
    magnify(0, 1)
    for _ in 0..<steps { magnify(per, 2) }
    magnify(0, 4)
case "pan":
    // A phased vertical pixel scroll (`pan x y dy [steps]`), then momentum
    // (kCGScrollWheelEventMomentumPhase: begin 1, continue 2, end 3);
    // `pan x y dy steps x2 y2` moves the pointer to x2 y2 before the momentum.
    post(.mouseMoved, CGPoint(x: num(2), y: num(3)))
    usleep(30_000)
    let steps = args.count > 5 ? Int(args[5])! : 16
    let step = Int32(num(4) / Double(steps))
    func vertical(_ dy: Int32, phase: Int64, momentum: Int64) {
        let e = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 1, wheel1: dy, wheel2: 0, wheel3: 0)!
        e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        e.setIntegerValueField(.scrollWheelEventScrollPhase, value: phase)
        e.setIntegerValueField(.scrollWheelEventMomentumPhase, value: momentum)
        e.post(tap: .cghidEventTap)
        usleep(12_000)
    }
    vertical(0, phase: 1, momentum: 0)
    for _ in 0..<steps { vertical(step, phase: 2, momentum: 0) }
    vertical(0, phase: 4, momentum: 0)
    if args.count > 7 { post(.mouseMoved, CGPoint(x: num(6), y: num(7))) }
    vertical(step, phase: 0, momentum: 1)
    for i in 0..<10 { vertical(step * Int32(10 - i) / 10, phase: 0, momentum: 2) }
    vertical(0, phase: 0, momentum: 3)
case "smartzoom":
    // A two-finger double tap: a gesture event carrying a zoom toggle (HID type 22).
    let p = CGPoint(x: num(2), y: num(3))
    post(.mouseMoved, p)
    usleep(30_000)
    if let e = CGEvent(source: source) {
        e.type = CGEventType(rawValue: 29)!
        e.location = p
        e.setIntegerValueField(CGEventField(rawValue: 110)!, value: 22)
        e.post(tap: .cghidEventTap)
    }
default:
    fputs("unknown command\n", stderr)
    exit(2)
}
usleep(50_000)
