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

## Portal backend

Only the session-bus owner of `org.freedesktop.portal.Desktop`, running the
approved `xdg-desktop-portal` executable as the same Unix user, can invoke the
backend. Standard `/usr/lib` and `/usr/libexec` installations are recognized.
For a custom installation, set `HALLEY_PORTAL_FRONTEND` to its absolute
executable path in the **portal backend's** service environment. Do not approve
an interpreter or a generic command runner. Requests and sessions remain bound
to the initiating unique bus connection; sessions also retain their app ID.

## Sensitive Wayland protocols

All ordinary connections start without desktop-wide capabilities. Before
launching Halley, set a colon-separated list of **absolute executable paths**
for each capability you intend to grant:

| Environment variable | Capability | Example trusted tool |
| --- | --- | --- |
| `HALLEY_ALLOW_SCREEN_CAPTURE` | Direct wlr-screencopy capture | `/usr/bin/grim` |
| `HALLEY_ALLOW_CLIPBOARD_CONTROL` | Clipboard/primary-selection monitoring | `/usr/bin/wl-paste` |
| `HALLEY_ALLOW_VIRTUAL_KEYBOARD` | Synthetic keyboard input | `/usr/bin/wtype` |
| `HALLEY_ALLOW_INPUT_METHOD` | IME registration and keyboard grabs | `/usr/bin/fcitx5` |

For example: `HALLEY_ALLOW_INPUT_METHOD=/usr/bin/fcitx5 halley`.
There are no implicit grants, basename matches, or wildcards. The compositor
compares the connecting peer's executable device/inode against the approved
files. Grants are fixed for that connection. Restart Halley and reconnect the
tool after changing the policy. A copied executable does not inherit a grant.
Never approve a shell, interpreter, generic application launcher, or sandbox
broker: its children or delegated sockets may carry its authority. Approved
programs themselves must be trusted, including their plugins and environment.

Normal focused clipboard copy/paste and text-input clients remain available.
Waybar's ordinary panel/tray operation needs none of these grants. Portal
capture uses Halley's separate compositor IPC, so applications should prefer
the portal's consent flow over unrestricted raw capture. The private same-user
IPC socket remains a trusted desktop administration interface; do not expose
it to untrusted sandboxes. This is not isolation from arbitrary unsandboxed
programs that can execute code as your user or modify your session setup.
