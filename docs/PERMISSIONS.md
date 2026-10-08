# Permission setup

Synkflow asks only for what it needs and shows the current state in **Settings → Diagnostics** and during first-run.

## macOS

| Permission | Why | Where |
|---|---|---|
| **Accessibility** | Lets Synkflow *swallow* local input while another computer is controlled, and post input when this Mac is controlled. | System Settings → Privacy & Security → Accessibility |
| **Input Monitoring** | Lets it notice your pointer reaching a screen edge and your key presses (to forward them). | System Settings → Privacy & Security → Input Monitoring |

The app shows each state, can trigger the system prompt ("Ask for access"), opens the right Settings pane, and re-checks when
you come back. Revocation is detected: the event tap reports it, sharing pauses, held keys are released, and a notice explains
what to do. Notes:

* The grant is tied to the app's code signature. An unsigned or ad-hoc-signed build is a *different* app after each rebuild and
  is asked again; a Developer-ID-signed build keeps it across updates.
* Secure-input fields (password boxes) and the login window cannot be controlled. Synkflow does not try.
* If you run from a terminal during development, macOS attributes the permission to the terminal.

## Windows

No special permission. Limits: elevated (Administrator) windows, UAC prompts and the lock screen ignore injected input (UIPI).
Synkflow does not run elevated and does not bypass this. Windows Firewall will ask once to allow Synkflow on **private**
networks — allow it only there. Synkflow never changes firewall rules by itself.

## Linux

* **X11**: no permission dialog. Needs the XInput2, XTEST and RandR extensions (standard).
* **Wayland**: keyboard/mouse sharing is unavailable (see `CAPABILITIES.md`); everything else works.
* The Secret Service (GNOME Keyring / KWallet) is used for the identity key when running; otherwise a `0600` file.
* Firewall: allow the listening port (default TCP 24847, shown in Settings → Network) on your LAN zone.
