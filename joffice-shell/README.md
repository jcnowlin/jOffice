# jOffice Shell

Native GTK4 + WebKitGTK 6.0 host for the Euro-Office web-apps editor.
Replaces the Qt/CEF DesktopEditors shell.

## Architecture

- The editor (web-apps JS + sdkjs) runs inside WebKitGTK.
- The shell provides native window chrome, file dialogs, and a JS bridge
  (`window.joffice.invoke`) for operations the web page cannot do alone.
- `Engine` trait abstracts the web engine: WebKitGTK 6.0 today, Servo
  when it matures.

## Build

```sh
cargo build --release
```

Needs GTK4 + WebKitGTK 6.0 dev libraries.

## Run

```sh
# Local web-apps (offline mode)
./target/debug/joffice-shell

# From a DocumentServer (cloud mode)
./target/debug/joffice-shell --url https://office.jcn

# Open a local file
./target/debug/joffice-shell --file doc.docx
```

## Bridge API (JS -> Rust)

- `joffice.invoke("openFile")` — native file picker, returns `{path}`
- `joffice.invoke("saveFile")` — native save dialog, returns `{path}`
- `joffice.invoke("ping")` — returns `{pong: true, engine: "webkit6"}`
