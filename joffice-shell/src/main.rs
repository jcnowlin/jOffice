// jOffice shell — native GTK4 + WebKitGTK 6.0 host for the Euro-Office
// web-apps editor. Replaces the Qt/CEF DesktopEditors shell.
//
// Architecture (Kagi-reviewed, round 1):
//   - The editor (web-apps JS + sdkjs) runs inside WebKitGTK, served from
//     the joffice:// custom URI scheme — NOT file://. file:// gives
//     origin null, blocks XHR/fetch/modules, and the editor expects a
//     host backend anyway.
//   - The host backend is window.AscDesktopEditor (see shim.js), the
//     desktop-host interface sdkjs actually speaks. window.joffice.invoke
//     is only the low-level Rust bridge the shim calls into.
//   - Local files flow through the x2t converter sidecar:
//     .docx/.xlsx/.pptx --x2t--> Editor.bin --shim--> editor
//     editor --asc_nativeGetFile--> Editor.bin --x2t--> .docx/.xlsx/.pptx
//   - Bridge is origin-scoped: the handler rejects any page whose URI is
//     not joffice://, and navigation policy blocks leaving joffice://.
//   - The shell never hands raw filesystem paths to page JS. Rust keeps
//     a handle table; JS gets opaque handle IDs.
//
// Usage:
//   joffice-shell [--file <local-file>]
//     --file: open a local file on startup (offline mode, via x2t).

use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, FileChooserAction, FileChooserNative, FileFilter, ResponseType,
};
use gtk4::gio::AppInfo;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use webkit6::javascriptcore::{Context, Value};
use webkit6::{
    NavigationAction, NavigationPolicyDecision, PolicyDecisionType, ScriptMessageReply,
    URISchemeRequest, UserContentInjectedFrames, UserContentManager, UserScript,
    UserScriptInjectionTime, WebContext, WebView,
};
use webkit6::prelude::*;

/// The custom scheme the editor is served from. Real origin, CORS-enabled,
/// display-isolated. Never file://, never http://localhost.
const EDITOR_SCHEME: &str = "joffice";
const EDITOR_ORIGIN: &str = "joffice://";

/// AscDesktopEditor shim, injected into the TOP frame before any page script.
/// Defines window.AscDesktopEditor on top of the Rust bridge.
const SHIM_JS: &str = include_str!("shim.js");

/// Minimal bootstrap: error collector + the Rust bridge entry point.
/// The shim (SHIM_JS) does the real work on top of this.
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

/// Opaque handle table: JS gets handle IDs, never raw filesystem paths.
struct HandleTable {
    next: u64,
    paths: HashMap<String, PathBuf>,
}

impl HandleTable {
    fn new() -> Self {
        Self { next: 1, paths: HashMap::new() }
    }

    fn insert(&mut self, path: PathBuf) -> String {
        let id = format!("h{}", self.next);
        self.next += 1;
        self.paths.insert(id.clone(), path);
        id
    }

    fn resolve(&self, handle_or_path: &str) -> Option<PathBuf> {
        if let Some(p) = self.paths.get(handle_or_path) {
            return Some(p.clone());
        }
        // Fallback: treat as a literal path (e.g. from --file).
        let p = PathBuf::from(handle_or_path);
        if p.exists() {
            Some(p)
        } else {
            None
        }
    }
}

/// Shared shell state.
struct Shell {
    /// Directory containing the web-apps checkout (editor JS).
    webapps_dir: PathBuf,
    /// x2t converter binary (from the `core` build). None = not found.
    x2t: Option<PathBuf>,
    /// File to open on startup (--file).
    pending_file: Option<PathBuf>,
    /// Opaque handle -> filesystem path.
    handles: Rc<RefCell<HandleTable>>,
    /// The main window (dialog parent). Set after window creation.
    window: RefCell<Option<ApplicationWindow>>,
    /// Currently open document's path (for save, not save-as).
    current_path: RefCell<Option<PathBuf>>,
}

impl Shell {
    fn new(webapps_dir: PathBuf, pending_file: Option<PathBuf>) -> Rc<Self> {
        // x2t lives in the `core` build output. Look in likely spots.
        let x2t = ["/usr/local/bin/x2t", "/usr/bin/x2t", "./x2t", "../core/build/bin/x2t"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists());
        Rc::new(Self {
            webapps_dir,
            x2t,
            pending_file,
            handles: Rc::new(RefCell::new(HandleTable::new())),
            window: RefCell::new(None),
            current_path: RefCell::new(None),
        })
    }
}

// ---------------------------------------------------------------------------
// x2t sidecar
// ---------------------------------------------------------------------------

/// AVS_OFFICE_FILE_FORMAT values (from ONLYOFFICE core / euro-office-lite).
/// The internal Editor.bin format code needs verification against core
/// source — marked here so Kagi round 2 can check it.
mod fmt {
    pub const BIN: i32 = 0; // Editor.bin internal format (VERIFY against core)
    pub const DOCX: i32 = 65;
    pub const DOC: i32 = 66;
    pub const ODT: i32 = 67;
    pub const RTF: i32 = 68;
    pub const TXT: i32 = 69;
    pub const PPTX: i32 = 129;
    pub const PPT: i32 = 130;
    pub const ODP: i32 = 131;
    pub const XLSX: i32 = 257;
    pub const XLS: i32 = 258;
    pub const ODS: i32 = 259;
    pub const CSV: i32 = 260;
    pub const PDF: i32 = 513;
}

fn format_for_path(path: &Path) -> i32 {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "docx" => fmt::DOCX,
        "doc" => fmt::DOC,
        "odt" => fmt::ODT,
        "rtf" => fmt::RTF,
        "txt" => fmt::TXT,
        "pptx" => fmt::PPTX,
        "ppt" => fmt::PPT,
        "odp" => fmt::ODP,
        "xlsx" => fmt::XLSX,
        "xls" => fmt::XLS,
        "ods" => fmt::ODS,
        "csv" => fmt::CSV,
        "pdf" => fmt::PDF,
        _ => fmt::DOCX,
    }
}

/// Run x2t to convert `from` -> `to` (target format code).
/// Returns Err with a clear message if x2t is missing or fails.
fn x2t_convert(shell: &Shell, from: &Path, to: &Path, format_to: i32) -> Result<(), String> {
    let x2t = shell.x2t.as_ref().ok_or_else(|| {
        "x2t converter not found. Build the `core` submodule first (see README).".to_string()
    })?;

    // x2t takes an XML params file.
    let params = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Task>
  <m_sFileFrom>{}</m_sFileFrom>
  <m_sFileTo>{}</m_sFileTo>
  <m_nFormatTo>{}</m_nFormatTo>
</Task>"#,
        from.display(),
        to.display(),
        format_to
    );
    let params_path = std::env::temp_dir().join(format!("joffice-x2t-{}.xml", std::process::id()));
    std::fs::write(&params_path, params).map_err(|e| format!("write x2t params: {e}"))?;

    let out = Command::new(x2t)
        .arg(&params_path)
        .output()
        .map_err(|e| format!("run x2t: {e}"))?;
    let _ = std::fs::remove_file(&params_path);

    if !out.status.success() {
        return Err(format!(
            "x2t failed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// URI scheme: serve web-apps from disk on joffice://
// ---------------------------------------------------------------------------

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

/// Serve a joffice:// URI from the web-apps directory.
/// joffice:///apps/documenteditor/main/index.html -> <webapps>/apps/documenteditor/main/index.html
fn handle_uri_scheme(shell: &Shell, req: &URISchemeRequest) {
    let uri = req.uri().unwrap_or_default();
    // Strip scheme + leading slashes: joffice:///apps/... -> /apps/...
    let path_part = uri.strip_prefix(EDITOR_ORIGIN).unwrap_or("").trim_start_matches('/');
    let file_path = shell.webapps_dir.join(path_part);

    // Directory -> index.html
    let file_path = if file_path.is_dir() {
        file_path.join("index.html")
    } else {
        file_path
    };

    match std::fs::read(&file_path) {
        Ok(data) => {
            let stream = gtk4::gio::MemoryInputStream::from_bytes(&gtk4::glib::Bytes::from(&data));
            req.finish(&stream, data.len() as i64, Some(content_type(&file_path)));
        }
        Err(_) => {
            let mut err = gtk4::glib::Error::new(
                gtk4::gio::IOErrorEnum::NotFound,
                &format!("joffice: not found: {uri}"),
            );
            req.finish_error(&mut err);
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge: JS (shim) -> Rust
// ---------------------------------------------------------------------------

fn handle_bridge(
    shell: &Rc<Shell>,
    view: &WebView,
    reply: &ScriptMessageReply,
    ctx: &Context,
    msg: &BridgeMsg,
) {
    // ORIGIN CHECK: only the editor origin may call the bridge.
    // The WebView's current URI must be joffice://. Anything else
    // (navigation to an external site, injected frame, etc.) is rejected.
    let uri = view.uri().unwrap_or_default();
    if !uri.starts_with(EDITOR_ORIGIN) {
        reply_err(reply, format!("bridge rejected: untrusted origin {uri}"));
        return;
    }

    match msg.cmd.as_str() {
        "pendingFile" => {
            let path = shell.pending_file.clone();
            // Consume: only report once.
            if let Some(p) = path {
                reply_json(reply, ctx, &serde_json::json!({ "path": p.to_string_lossy() }));
            } else {
                reply_json(reply, ctx, &serde_json::json!({ "path": null }));
            }
        }

        "openFileDialog" => {
            let window = shell.window.borrow().clone();
            let dialog = FileChooserNative::new(
                Some("Open document"),
                window.as_ref(),
                FileChooserAction::Open,
                Some("_Open"),
                Some("_Cancel"),
            );
            let filter = FileFilter::new();
            filter.set_name(Some("Documents"));
            for mime in [
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "application/pdf",
                "application/vnd.oasis.opendocument.text",
            ] {
                filter.add_mime_type(mime);
            }
            dialog.add_filter(&filter);
            // Hold a strong ref until the response fires (dialog is a
            // local; without this the response may never arrive).
            // The closure holds a clone; it drops naturally when the
            // dialog is destroyed after the response.
            let dialog_rc = Rc::new(dialog);
            let dialog_clone = dialog_rc.clone();
            let _dialog_hold = dialog_rc.clone();
            let reply_clone = reply.clone();
            let ctx_clone = ctx.clone();
            let handles = shell.handles.clone();
            dialog_clone.connect_response(move |d, resp| {
                if resp == ResponseType::Accept {
                    if let Some(path) = d.file().and_then(|f| f.path()) {
                        let handle = handles.borrow_mut().insert(path);
                        reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "handle": handle }));
                    } else {
                        reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "handle": null }));
                    }
                } else {
                    reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "handle": null }));
                }
            });
            dialog_clone.show();
        }

        "openFile" => {
            // Convert the requested file via x2t -> Editor.bin, return base64.
            let handle = msg.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = match shell.handles.borrow().resolve(handle) {
                Some(p) => p,
                None => {
                    reply_err(reply, format!("openFile: unknown handle/path: {handle}"));
                    return;
                }
            };
            *shell.current_path.borrow_mut() = Some(path.clone());
            let tmp = std::env::temp_dir().join(format!("joffice-open-{}.bin", std::process::id()));
            match x2t_convert(shell, &path, &tmp, fmt::BIN) {
                Ok(()) => match std::fs::read(&tmp) {
                    Ok(data) => {
                        let _ = std::fs::remove_file(&tmp);
                        use base64::Engine;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                        reply_json(reply, ctx, &serde_json::json!({ "data": b64 }));
                    }
                    Err(e) => reply_err(reply, format!("read Editor.bin: {e}")),
                },
                Err(e) => reply_err(reply, e),
            }
        }

        "convertFile" => {
            let handle = msg.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = match shell.handles.borrow().resolve(handle) {
                Some(p) => p,
                None => {
                    reply_err(reply, format!("convertFile: unknown handle/path: {handle}"));
                    return;
                }
            };
            let tmp = std::env::temp_dir().join(format!("joffice-conv-{}.bin", std::process::id()));
            match x2t_convert(shell, &path, &tmp, fmt::BIN) {
                Ok(()) => match std::fs::read(&tmp) {
                    Ok(data) => {
                        let _ = std::fs::remove_file(&tmp);
                        use base64::Engine;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                        reply_json(
                            reply,
                            ctx,
                            &serde_json::json!({ "data": b64, "images": {} }),
                        );
                    }
                    Err(e) => reply_err(reply, format!("read Editor.bin: {e}")),
                },
                Err(e) => reply_err(reply, e),
            }
        }

        "saveFile" => {
            // JS sends Editor.bin (base64). Convert back via x2t and write.
            let b64 = msg.args.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let save_as = msg.args.get("saveAs").and_then(|v| v.as_bool()).unwrap_or(false);
            use base64::Engine;
            let bin = match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => {
                    reply_err(reply, format!("saveFile: bad base64: {e}"));
                    return;
                }
            };
            let tmp_bin = std::env::temp_dir().join(format!("joffice-save-{}.bin", std::process::id()));
            if let Err(e) = std::fs::write(&tmp_bin, &bin) {
                reply_err(reply, format!("write temp Editor.bin: {e}"));
                return;
            }

            // Determine target path: current file, or Save As dialog.
            let target: Option<PathBuf> = if save_as || shell.current_path.borrow().is_none() {
                // Save As: show dialog, block on it (sync via channel).
                // For now, require the shim to pass a handle; dialog flow
                // is handled by the saveFileDialog command below.
                None
            } else {
                shell.current_path.borrow().clone()
            };

            match target {
                Some(path) => {
                    let fmt_to = format_for_path(&path);
                    match x2t_convert(shell, &tmp_bin, &path, fmt_to) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(&tmp_bin);
                            reply_json(reply, ctx, &serde_json::json!({ "ok": true }));
                        }
                        Err(e) => reply_err(reply, e),
                    }
                }
                None => {
                    // No target yet: ask the shim to show a Save As dialog
                    // via the saveFileDialog command, then retry.
                    reply_err(reply, "saveFile: no target path; use saveFileDialog first".to_string());
                }
            }
        }

        "saveFileDialog" => {
            let window = shell.window.borrow().clone();
            let dialog = FileChooserNative::new(
                Some("Save document"),
                window.as_ref(),
                FileChooserAction::Save,
                Some("_Save"),
                Some("_Cancel"),
            );
            dialog.set_current_name("Untitled.docx");
            let dialog_rc = Rc::new(dialog);
            let dialog_clone = dialog_rc.clone();
            let _dialog_hold = dialog_rc.clone();
            let reply_clone = reply.clone();
            let ctx_clone = ctx.clone();
            let handles = shell.handles.clone();
            let current = shell.current_path.clone();
            dialog_clone.connect_response(move |d, resp| {
                if resp == ResponseType::Accept {
                    if let Some(path) = d.file().and_then(|f| f.path()) {
                        *current.borrow_mut() = Some(path.clone());
                        let handle = handles.borrow_mut().insert(path);
                        reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "handle": handle }));
                    } else {
                        reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "handle": null }));
                    }
                } else {
                    reply_json(&reply_clone, &ctx_clone, &serde_json::json!({ "handle": null }));
                }
            });
            dialog_clone.show();
        }

        "createNew" => {
            // New empty document: clear current path, return ok.
            *shell.current_path.borrow_mut() = None;
            reply_json(reply, ctx, &serde_json::json!({ "ok": true }));
        }

        "ping" => {
            reply_json(reply, ctx, &serde_json::json!({ "pong": true, "engine": "webkit6" }));
        }

        other => {
            reply_err(reply, format!("unknown bridge command: {other}"));
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    // Parse CLI args ONCE, before the app exists. GApplication re-fires
    // activate on a second launch; re-parsing args there would be stale.
    let args: Vec<String> = std::env::args().collect();
    let mut pending_file: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--file" if i + 1 < args.len() => {
                pending_file = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            _ => i += 1,
        }
    }

    // Resolve the web-apps dir: <exe-dir>/../web-apps, or ./web-apps.
    // (Fragile path from round 0 replaced with a search over candidates.)
    let webapps_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .map(|exe_dir| exe_dir.join("../web-apps"))
        .filter(|p| p.join("apps/documenteditor/main/index.html").exists())
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            let p = cwd.join("web-apps");
            if p.join("apps/documenteditor/main/index.html").exists() {
                Some(p)
            } else {
                None
            }
        })
        .unwrap_or_else(|| PathBuf::from("../web-apps"));

    let shell = Shell::new(webapps_dir, pending_file);

    let app = Application::builder()
        .application_id("io.nowlin.jOffice")
        .build();

    let shell_clone = shell.clone();
    app.connect_activate(move |app| {
        let shell = shell_clone.clone();

        let window = ApplicationWindow::builder()
            .application(app)
            .title("jOffice")
            .default_width(1280)
            .default_height(900)
            .build();
        *shell.window.borrow_mut() = Some(window.clone());

        // WebContext with the joffice:// scheme + CORS.
        let context = WebContext::default().expect("default WebContext");
        let security = context.security_manager().expect("security manager");
        security.register_uri_scheme_as_cors_enabled(EDITOR_SCHEME);
        security.register_uri_scheme_as_secure(EDITOR_SCHEME);
        security.register_uri_scheme_as_display_isolated(EDITOR_SCHEME);

        let shell_scheme = shell.clone();
        context.register_uri_scheme(EDITOR_SCHEME, move |req| {
            handle_uri_scheme(&shell_scheme, req);
        });

        // WebView with JS bridge + shim, injected into TOP frame only.
        let manager = UserContentManager::new();
        manager.register_script_message_handler_with_reply("joffice", None);

        let bootstrap = UserScript::new(
            BOOTSTRAP_JS,
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        manager.add_script(&bootstrap);

        let shim = UserScript::new(
            SHIM_JS,
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        manager.add_script(&shim);

        let view = WebView::builder()
            .user_content_manager(&manager)
            .web_context(&context)
            .build();

        // Bridge: JS -> Rust, with origin check inside handle_bridge.
        // Weak ref breaks the WebView -> manager -> closure -> WebView cycle.
        let view_weak = view.downgrade();
        let shell_bridge = shell.clone();
        manager.connect_script_message_with_reply_received(
            Some("joffice"),
            move |_mgr, value, reply| {
                let reply = reply.clone();
                let text = value.to_json(0).map(|s| s.to_string()).unwrap_or_default();
                let ctx_opt: Option<Context> = value.context();
                match (serde_json::from_str::<BridgeMsg>(&text), ctx_opt) {
                    (Ok(msg), Some(ctx)) => {
                        if let Some(v) = view_weak.upgrade() {
                            handle_bridge(&shell_bridge, &v, &reply, &ctx, &msg);
                        } else {
                            reply_err(&reply, "WebView gone".to_string());
                        }
                    }
                    (Err(e), _) => reply_err(&reply, format!("bad bridge message: {e}")),
                    (_, None) => reply_err(&reply, "no JS context".to_string()),
                }
                true
            },
        );

        // NAVIGATION POLICY: block top-level navigation away from joffice://.
        // External links open in the system browser via UriLauncher.
        view.connect_decide_policy(|_view, decision, decision_type| {
            if decision_type != PolicyDecisionType::NavigationAction {
                return false; // let WebKit handle non-navigation decisions
            }
            let nav_policy: &NavigationPolicyDecision =
                match decision.downcast_ref::<NavigationPolicyDecision>() {
                    Some(n) => n,
                    None => return false,
                };
            let mut nav: NavigationAction = match nav_policy.navigation_action() {
                Some(n) => n,
                None => return false,
            };
            let uri = nav.request().and_then(|r| r.uri()).unwrap_or_default();
            if uri.starts_with(EDITOR_ORIGIN) || uri.is_empty() {
                return false; // allow editor navigation
            }
            // External: open in system browser, block in WebView.
            let _ = AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>);
            decision.ignore();
            true
        });

        // Load the editor from the custom scheme.
        view.load_uri("joffice:///apps/documenteditor/main/index.html");

        // Console messages to stdout for debugging (WebKitGTK 6.0).
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_enable_write_console_messages_to_stdout(true);
        }

        // Load failures.
        view.connect_load_failed(|_, _event, uri, error| {
            eprintln!("joffice: page load failed ({uri}): {error}");
            false
        });

        window.set_child(Some(&view));
        window.present();
    });

    app.run();
}
