import AppKit

// Entry point. The app is built without a nib: the menu is created in AppDelegate.
let app = NSApplication.shared
let appDelegate = AppDelegate()
app.delegate = appDelegate
app.setActivationPolicy(.regular)
app.run()
