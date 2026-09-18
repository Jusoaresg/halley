# Desktop capability permissions

Halley does not treat permission to display a window as permission to read the
whole desktop or inject keyboard input.

## Accessibility keyboard monitor

Keyboard monitoring is denied by default. To approve a screen reader, first
start it on the session bus, determine its unique connection name using
`busctl --user status org.gnome.Orca.KeyboardMonitor`, and launch Halley with
`HALLEY_ACCESSIBILITY_MONITOR=:1.42` (substitute that connection's unique name).
The approved connection must also own `org.gnome.Orca.KeyboardMonitor`.
A well-known name, wildcard, executable name, or replacement connection is not
an approval. Restarting the screen reader requires a new explicit approval.
Do not set this variable from an untrusted application's output automatically.

These controls constrain protocol access, not arbitrary execution as your Unix
user. Applications able to modify your session environment or compositor
configuration already share your desktop's administrative trust boundary.
