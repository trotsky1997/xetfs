import AppKit

let alert = NSAlert()
alert.messageText = "XetFS"
alert.informativeText = "Enable the file system extension in System Settings, then use mount-directory to mount a repository at an empty directory."
alert.runModal()
