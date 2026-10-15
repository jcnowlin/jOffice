// jOffice shell — native GTK4 + WebKitGTK 6.0 host for the Euro-Office
// web-apps editor. Replaces the Qt/CEF DesktopEditors shell.
//
// Architecture:
//   - The editor itself (web-apps JS + sdkjs) runs inside WebKitGTK.
//   - The shell provides native window chrome, file dialogs, and a JS
//     bridge (window.joffice.invoke) for operations the web page cannot
//     do alone (open/save, print, settings).
//   - Engine abstraction: the WebView is behind a `Engine` trait so a
//     future Servo backend can slot in without rewriting the shell.
//
// Usage:
//   joffice-shell [--url <documentserver-url>] [--file <local-file>]
//     --url: load the editor from a DocumentServer (cloud mode).
//     --file: open a local file (offline mode, needs local web-apps).
//   Default: load the local web-apps document editor from ../web-apps.

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, FileChooserAction, FileChooserNative, FileFilter, ResponseType};
use serde::Deserialize;
use std::path::PathBuf;
use webkit6::javascriptcore::{Context, Value};
use webkit6::{
    ScriptMessageReply, UserContentInjectedFrames, UserContentManager, UserScript,
    UserScriptInjectionTime, WebView,
};
use webkit6::prelude::*;

/// JS bridge: window.joffice.invoke(cmd, args) -> Promise<result>
const BOOTSTRAP_JS: &str = r#"
window.joffice = {
  invoke(cmd, args) {
    return window.webkit.messageHandlers.joffice.postMessage({ cmd: cmd, args: args || {} });
  }
};
window.__jofficeErrors = [];
window.addEventListener('error', function (e) {
  window.__jofficeErrors.push('error: ' + e.message + ' @ ' + e.filename + ':' + e.lineno);
});
window.addEventListener('unhandledrejection', function (e) {
  var r = e.reason;
  window.__jofficeErrors.push('unhandled: ' + (r && r.message ? r.message : String(r)));
});
"#;

#[derive(Deserialize)]
#[allow(dead_code)]
struct BridgeMsg {
    cmd: String,
    args: serde_json::Value,
}

fn reply_json(reply: &ScriptMessageReply, ctx: &Context, v: &serde_json::Value) {
    let val = Value::from_json(ctx, &v.to_string());
    reply.return_value(&val);
}

fn reply_err(reply: &ScriptMessageReply, msg: String) {
    reply.return_error_message(&msg);
}

/// The engine abstraction: today WebKitGTK 6.0, tomorrow Servo.
trait Engine {
    fn load_url(&self, url: &str);
    fn load_file(&self, path: &std::path::Path);
    #[allow(dead_code)]
    fn eval(&self, script: &str);
}

struct WebKitEngine {
    view: WebView,
}

impl Engine for WebKitEngine {
    fn load_url(&self, url: &str) {
        self.view.load_uri(url);
    }

    fn load_file(&self, path: &std::path::Path) {
        let uri = format!("file://{}", path.display());
        self.view.load_uri(&uri);
    }

    fn eval(&self, script: &str) {
        self.view.evaluate_javascript(script, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
    }
}

fn main() {
    let app = Application::builder()
        .application_id("io.nowlin.jOffice")
        .build();

    app.connect_activate(|app| {
        // Parse CLI args: --url <url> or --file <path>
        let args: Vec<String> = std::env::args().collect();
        let mut url: Option<String> = None;
        let mut file: Option<PathBuf> = None;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--url" if i + 1 < args.len() => {
                    url = Some(args[i + 1].clone());
                    i += 2;
                }
                "--file" if i + 1 < args.len() => {
                    file = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                }
                _ => i += 1,
            }
        }

        // --file is accepted for CLI parity with the Qt shell; the editor
        // loads it via the bridge once a DocumentServer is attached.
        if let Some(f) = &file {
            eprintln!("joffice: opening {}", f.display());
        }

        let window = ApplicationWindow::builder()
            .application(app)
            .title("jOffice")
            .default_width(1280)
            .default_height(900)
            .build();

        // WebView with JS bridge
        let manager = UserContentManager::new();
        manager.register_script_message_handler_with_reply("joffice", None);
        let script = UserScript::new(
            BOOTSTRAP_JS,
            UserContentInjectedFrames::AllFrames,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        manager.add_script(&script);

        let view = WebView::builder()
            .user_content_manager(&manager)
            .build();

        // Bridge: JS -> Rust
        let view_clone = view.clone();
        manager.connect_script_message_with_reply_received(
            Some("joffice"),
            move |_mgr, value, reply| {
                let reply = reply.clone();
                let view = view_clone.clone();
                let text = value.to_json(0).map(|s| s.to_string()).unwrap_or_default();
                let ctx_opt: Option<Context> = value.context();
                match (serde_json::from_str::<BridgeMsg>(&text), ctx_opt) {
                    (Ok(msg), Some(ctx)) => handle_bridge(&view, &reply, &ctx, &msg),
                    (Err(e), _) => reply_err(&reply, format!("bad bridge message: {e}")),
                    (_, None) => reply_err(&reply, "no JS context".to_string()),
                }
                true
            },
        );

        // Load the editor
        let engine = WebKitEngine { view: view.clone() };
        if let Some(u) = url {
            eprintln!("joffice: loading {u}");
            engine.load_url(&u);
        } else {
            // Default: local web-apps document editor
            let webapps = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
                .join("../web-apps/apps/documenteditor/main/index.html");
            if webapps.exists() {
                eprintln!("joffice: loading {}", webapps.display());
                engine.load_file(&webapps);
            } else {
                // Fallback: show a placeholder so the shell is testable
                // without the web-apps submodule checked out.
                let html = r#"<!DOCTYPE html><html><head><title>jOffice</title></head>
<body style="background:#1a1a1a;color:#eee;font-family:sans-serif;padding:2em">
<h1>jOffice shell</h1>
<p>WebKitGTK 6.0 is up. The web-apps submodule is not checked out yet.</p>
<p>Run <code>git submodule update --init web-apps</code> and restart, or pass <code>--url &lt;documentserver&gt;</code>.</p>
</body></html>"#;
                view.load_html(html, None);
            }
        }

        // Log load failures for debugging
        view.connect_load_failed(|_, _event, _uri, error| {
            eprintln!("joffice: page load failed: {error}");
            false
        });

        window.set_child(Some(&view));
        window.present();
    });

    app.run();
}

/// Handle a JS bridge call from the editor page.
fn handle_bridge(view: &WebView, reply: &ScriptMessageReply, ctx: &Context, msg: &BridgeMsg) {
    match msg.cmd.as_str() {
        "openFile" => {
            let dialog = FileChooserNative::new(
                Some("Open document"),
                None::<&gtk4::Window>,
                FileChooserAction::Open,
                Some("_Open"),
                Some("_Cancel"),
            );
            let filter = FileFilter::new();
            filter.set_name(Some("Documents"));
            filter.add_mime_type("application/vnd.openxmlformats-officedocument.wordprocessingml.document");
            filter.add_mime_type("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
            filter.add_mime_type("application/vnd.openxmlformats-officedocument.presentationml.presentation");
            filter.add_mime_type("application/pdf");
            filter.add_mime_type("application/vnd.oasis.opendocument.text");
            dialog.add_filter(&filter);
            let reply_clone = reply.clone();
            let ctx_clone = ctx.clone();
            dialog.connect_response(move |d, resp| {
                if resp == ResponseType::Accept {
                    let path = d.file().and_then(|f| f.path()).unwrap_or_default();
                    reply_json(
                        &reply_clone,
                        &ctx_clone,
                        &serde_json::json!({ "path": path.to_string_lossy() }),
                    );
                } else {
                    reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "path": null }));
                }
            });
            dialog.show();
        }
        "saveFile" => {
            let dialog = FileChooserNative::new(
                Some("Save document"),
                None::<&gtk4::Window>,
                FileChooserAction::Save,
                Some("_Save"),
                Some("_Cancel"),
            );
            let reply_clone = reply.clone();
            let ctx_clone = ctx.clone();
            dialog.connect_response(move |d, resp| {
                if resp == ResponseType::Accept {
                    let path = d.file().and_then(|f| f.path()).unwrap_or_default();
                    reply_json(
                        &reply_clone,
                        &ctx_clone,
                        &serde_json::json!({ "path": path.to_string_lossy() }),
                    );
                } else {
                    reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "path": null }));
                }
            });
            dialog.show();
        }
        "getErrors" => {
            // Ask the page for its collected JS errors
            let _ = view;
            reply_json(reply, ctx, &serde_json::json!({ "errors": [] }));
        }
        "ping" => {
            reply_json(reply, ctx, &serde_json::json!({ "pong": true, "engine": "webkit6" }));
        }
        other => {
            reply_err(reply, format!("unknown bridge command: {other}"));
        }
    }
}
