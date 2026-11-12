# Windows launcher

Double-click **Launch Codexify.cmd** in the repository to open the native launcher.
It starts the installed Codexify service, its saved OpenAI tunnel(s), and the
ChatGPT browser companion when browser automation is enabled. Your private local
chat opens in the default browser once startup succeeds.

To install a desktop and Start menu shortcut:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-windows-launcher.ps1 -Launch
```

The installer copies the launcher into `%USERPROFILE%\.codexify\launcher`, so the
shortcuts keep working if this checkout moves. Run the installer again to update
those copies. It requires no administrator rights and uses Windows PowerShell 5.1
and WPF, which are included with Windows.

The launcher always uses the existing `%USERPROFILE%\.codexify\bin\codexify.exe`
and `%USERPROFILE%\.codexify\codexify.config.json`, rather than the example config
in this checkout. Existing tunnel key references, projects, chat history and the
browser profile are reused. A first-time installation still needs
`codexify quickstart`. The launcher does not provision a new tunnel or connector.

- **Start all** starts the installed service and restores the browser session.
  Existing running services and ready/busy browser sessions are reused. It checks
  the local chat and tunnel health before reporting a successful connection.
- **Open chat** opens the authenticated local chat using the persistent owner
  token in `owner-chat.json`. That token is never displayed in the launcher.
- **Stop services** stops the service (including its supervised tunnels), then
  the browser companion. Use this when you intend to interrupt running work.
- **Browser** shows the existing ChatGPT browser window. **Sign in** opens its
  sign-in surface when the saved session has expired.
- **Launch at Windows sign-in** adds/removes only this launcher's per-user Startup
  shortcut. The existing Codexify service's own login setting is independent.
- **Open chat after startup** is remembered in `launcher\preferences.json`.

Closing the launcher leaves the services running. Opening another copy brings the
existing launcher to the front. Periodic status checks and service commands run in
a background runspace so the window stays responsive. Startup failures remain
visible in the activity area; the service logs are under `.codexify\logs`.
Command output draining is bounded after the CLI exits, so a persistent browser
that inherits output handles cannot leave startup waiting indefinitely. Live
health checks still decide whether the chat and tunnel are connected.

For a status-only launch, pass `-NoAutoStart` to `windows-launcher.ps1`. For a
non-operational visual preview, pass `-PreviewPath C:\path\preview.png`. Preview
mode starts no services and writes no preferences or shortcuts. `-CapturePath`
saves the actual window after its first completed action and leaves it open.

Run the isolated launcher checks with:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-windows-launcher.ps1
```
