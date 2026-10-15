// jOffice shell — native GTK4 + WebKitGTK 6.0 host for the Euro-Office
// web-apps editor. Replaces the Qt/CEF DesktopEditors shell.
//
// Architecture (Kagi-reviewed, rounds 1-2):
//   - The editor (web-apps JS + sdkjs) runs inside WebKitGTK, served from
//     the joffice:// custom URI scheme — NOT file://.
//   - The host backend is window.AscDesktopEditor (see shim.js), the
//     desktop-host interface sdkjs actually speaks. window.joffice.invoke
//     is only the low-level Rust bridge the shim calls into.
//   - Local files flow through the x2t converter sidecar (worker thread,
//     never the GTK main thread):
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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// No literal-path fallback — a path is only resolvable if it was
/// registered (via a dialog or --file at startup).
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

    fn resolve(&self, handle: &str) -> Option<PathBuf> {
        self.paths.get(handle).cloned()
    }

    #[allow(dead_code)]
    fn name_of(&self, handle: &str) -> Option<String> {
        self.paths
            .get(handle)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }
}

/// Shared shell state.
struct Shell {
    /// Directory containing the web-apps checkout (editor JS), canonicalized.
    webapps_dir: PathBuf,
    /// x2t converter binary (from the `core` build). None = not found.
    x2t: Option<PathBuf>,
    /// File to open on startup (--file). Consumed once via take().
    pending_file: RefCell<Option<PathBuf>>,
    /// Opaque handle -> filesystem path.
    handles: Rc<RefCell<HandleTable>>,
    /// The main window (dialog parent). Set after window creation.
    window: RefCell<Option<ApplicationWindow>>,
    /// Currently open document's path (for save, not save-as).
    current_path: RefCell<Option<PathBuf>>,
    /// Active file dialog, held to keep it alive until response.
    /// Cleared in the response handler, breaking the Shell->dialog->
    /// closure->Shell cycle. No Rc in the closure.
    active_dialog: RefCell<Option<FileChooserNative>>,
    /// Window close requested while a save is in flight. The close
    /// handler blocks (Propagation::Stop) and the save completion
    /// destroys the window. Prevents save-on-close data loss.
    close_pending: RefCell<bool>,
}

impl Shell {
    fn new(webapps_dir: PathBuf, pending_file: Option<PathBuf>) -> Rc<Self> {
        let webapps_dir = webapps_dir.canonicalize().unwrap_or(webapps_dir);
        let x2t = ["/usr/local/bin/x2t", "/usr/bin/x2t", "./x2t", "../core/build/bin/x2t"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists());
        let mut handles = HandleTable::new();
        // Register --file at startup so it is resolvable as a handle.
        if let Some(p) = &pending_file {
            handles.insert(p.clone());
        }
        Rc::new(Self {
            webapps_dir,
            x2t,
            pending_file: RefCell::new(pending_file),
            handles: Rc::new(RefCell::new(handles)),
            window: RefCell::new(None),
            current_path: RefCell::new(None),
            active_dialog: RefCell::new(None),
            close_pending: RefCell::new(false),
        })
    }
}

// ---------------------------------------------------------------------------
// x2t sidecar
// ---------------------------------------------------------------------------

/// AVS_OFFICE_FILE_FORMAT values (from ONLYOFFICE core OfficeFileFormats.h).
/// BIN: the internal Editor.bin format. 0 is AVS_OFFICESTUDIO_FILE_UNKNOWN
/// (a sentinel, NOT a target). 8192 (0x2000) is the reported internal binary
/// code — verify empirically: x2t output must start with the sdkjs stream
/// signature (AscCommon.c_oSerFormat.Signature).
mod fmt {
    pub const BIN: i32 = 8192; // Editor.bin internal format (VERIFY via signature)
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Unique temp stem: pid + atomic counter. Never pid alone — concurrent
/// conversions would clobber each other's params/output files, and
/// predictable temp names are a symlink hazard.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_stem(tag: &str) -> String {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("joffice-{tag}-{}-{n}", std::process::id())
}

/// Run x2t synchronously (call only from a worker thread).
/// Params XML root is TaskQueueDataConvert — Task with bare children is
/// not the schema x2t parses. Paths are escaped.
/// CSV/TXT get encoding/delimiter defaults (else x2t exits 89).
/// `format_from`: source format code. Required when the source is bin
/// (8192) — x2t cannot auto-detect the internal format.
/// Returns the output file bytes.
fn x2t_convert_sync(
    x2t: &Path,
    from: &Path,
    format_to: i32,
    format_from: Option<i32>,
) -> Result<Vec<u8>, String> {
    let stem = temp_stem("x2t");
    let tmpdir = std::env::temp_dir();
    let out_path = tmpdir.join(format!("{stem}.out"));
    let params_path = tmpdir.join(format!("{stem}.xml"));

    // x2t infers from extensions: the input extension must be right.
    // Copy the input to a temp file with its real extension if needed.
    let from_ext = from.extension().and_then(|e| e.to_str()).unwrap_or("docx");
    let from_tmp = tmpdir.join(format!("{stem}.in.{from_ext}"));
    std::fs::copy(from, &from_tmp).map_err(|e| format!("stage input: {e}"))?;

    let mut extra = String::new();
    if let Some(f) = format_from {
        extra.push_str(&format!("  <m_nFormatFrom>{f}</m_nFormatFrom>\n"));
    }
    if format_to == fmt::CSV || format_to == fmt::TXT {
        // CSV/TXT need encoding + delimiter or x2t exits 89.
        // Element name is m_nCsvDelimiter (not m_cCsvDelimiter).
        // Encoding 46 = reference value (not Windows codepage 65001).
        extra.push_str("  <m_nCsvTxtEncoding>46</m_nCsvTxtEncoding>\n");
        extra.push_str("  <m_nCsvDelimiter>4</m_nCsvDelimiter>\n");
    }

    let params = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <m_sFileFrom>{}</m_sFileFrom>
  <m_sFileTo>{}</m_sFileTo>
  <m_nFormatTo>{}</m_nFormatTo>
  <m_sTempDir>{}</m_sTempDir>
  <m_sFontDir />
  <m_sTitle>jOffice</m_sTitle>
  <m_bIsNoBase64>true</m_bIsNoBase64>
{}</TaskQueueDataConvert>"#,
        xml_escape(&from_tmp.display().to_string()),
        xml_escape(&out_path.display().to_string()),
        format_to,
        xml_escape(&tmpdir.display().to_string()),
        extra
    );
    std::fs::write(&params_path, params).map_err(|e| format!("write x2t params: {e}"))?;

    let out = Command::new(x2t)
        .arg(&params_path)
        .output()
        .map_err(|e| format!("run x2t: {e}"))?;

    let _ = std::fs::remove_file(&params_path);
    let _ = std::fs::remove_file(&from_tmp);

    if !out.status.success() {
        return Err(format!(
            "x2t failed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&out_path).map_err(|e| format!("read x2t output: {e}"))?;
    let _ = std::fs::remove_file(&out_path);
    Ok(bytes)
}

/// Run x2t on a worker thread; `done` runs on the GTK main thread with
/// the output bytes (or an error). Never blocks the UI.
fn x2t_convert_async(
    shell: &Rc<Shell>,
    from: PathBuf,
    format_to: i32,
    format_from: Option<i32>,
    done: impl FnOnce(Result<Vec<u8>, String>) + 'static,
) {
    let x2t = match shell.x2t.clone() {
        Some(x) => x,
        None => {
            done(Err(
                "x2t converter not found. Build the `core` submodule first (see README).".to_string(),
            ));
            return;
        }
    };
    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
    std::thread::spawn(move || {
        let result = x2t_convert_sync(&x2t, &from, format_to, format_from);
        let _ = tx.send(result);
    });
    // Poll the channel from the main thread; reply there (WebKit calls
    // must stay on main). Polling is ugly; glib::MainContext::channel
    // does not exist in this glib version.
    let mut done_opt = Some(done);
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        match rx.try_recv() {
            Ok(result) => {
                if let Some(done) = done_opt.take() {
                    done(result);
                }
                gtk4::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if let Some(done) = done_opt.take() {
                    done(Err("x2t worker died".to_string()));
                }
                gtk4::glib::ControlFlow::Break
            }
        }
    });
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
/// Defenses: percent-decode, strip query/fragment, canonicalize and
/// assert the result stays under webapps_dir (blocks ../../ traversal).
fn handle_uri_scheme(shell: &Shell, req: &URISchemeRequest) {
    let uri = req.uri().unwrap_or_default();
    let rest = uri.strip_prefix(EDITOR_ORIGIN).unwrap_or("");
    // Strip query and fragment: ?v=2 and #hash are not filesystem.
    let path_part = rest.split(['?', '#']).next().unwrap_or("");
    // Percent-decode: %20 -> space, etc.
    let decoded = gtk4::glib::uri_unescape_string(path_part, None::<&str>)
        .map(|s| s.to_string())
        .unwrap_or_else(|| path_part.to_string());
    let file_path = shell.webapps_dir.join(decoded.trim_start_matches('/'));

    let file_path = if file_path.is_dir() {
        file_path.join("index.html")
    } else {
        file_path
    };

    // Canonicalize and verify containment. Nonexistent -> 404 (safe).
    match file_path.canonicalize() {
        Ok(canon) if canon.starts_with(&shell.webapps_dir) => match std::fs::read(&canon) {
            Ok(data) => {
                let stream =
                    gtk4::gio::MemoryInputStream::from_bytes(&gtk4::glib::Bytes::from(&data));
                req.finish(&stream, data.len() as i64, Some(content_type(&canon)));
            }
            Err(_) => {
                let mut err = gtk4::glib::Error::new(
                    gtk4::gio::IOErrorEnum::NotFound,
                    &format!("joffice: not found: {uri}"),
                );
                req.finish_error(&mut err);
            }
        },
        Ok(_) => {
            // Traversal outside web-apps.
            let mut err = gtk4::glib::Error::new(
                gtk4::gio::IOErrorEnum::PermissionDenied,
                "joffice: path outside web-apps",
            );
            req.finish_error(&mut err);
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

// After a save completes, if a window close is pending, destroy the
// window now that the document is safely written. Called from the
// saveFile reply paths on the main thread.
fn maybe_destroy_after_save(shell: &Rc<Shell>) {
    if *shell.close_pending.borrow() {
        if let Some(win) = shell.window.borrow().as_ref() {
            win.destroy();
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
    // view.uri() is the top-frame URI; a same-origin iframe could in
    // principle reach the handler, but the shim is TopFrame-only and
    // the editor origin is ours. Known limitation, documented.
    let uri = view.uri().unwrap_or_default();
    if !uri.starts_with(EDITOR_ORIGIN) {
        reply_err(reply, format!("bridge rejected: untrusted origin {uri}"));
        return;
    }

    match msg.cmd.as_str() {
        "pendingFile" => {
            // CONSUME once: take() clears it so a second LocalStartOpen
            // does not re-open the same file.
            let path = shell.pending_file.borrow_mut().take();
            match path {
                Some(p) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let handle = shell.handles.borrow_mut().insert(p);
                    reply_json(
                        reply,
                        ctx,
                        &serde_json::json!({ "handle": handle, "name": name }),
                    );
                }
                None => {
                    reply_json(reply, ctx, &serde_json::json!({ "handle": null, "name": null }));
                }
            }
        }

        "openFileDialog" => {
            // A dialog is already open: reject instead of overwriting
            // active_dialog (which would drop the first dialog mid-flight).
            if shell.active_dialog.borrow().is_some() {
                reply_err(reply, "openFileDialog: dialog already open".to_string());
                return;
            }
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
            // Hold the dialog in Shell state (cleared on response).
            // No Rc in the closure: the closure captures `shell`, and
            // the response handler clears the field, breaking the cycle.
            *shell.active_dialog.borrow_mut() = Some(dialog.clone());
            let shell_c = shell.clone();
            let reply_c = reply.clone();
            let ctx_c = ctx.clone();
            dialog.connect_response(move |d, resp| {
                *shell_c.active_dialog.borrow_mut() = None;
                if resp == ResponseType::Accept {
                    if let Some(path) = d.file().and_then(|f| f.path()) {
                        let handle = shell_c.handles.borrow_mut().insert(path);
                        reply_json(&reply_c, &ctx_c, &serde_json::json!({ "handle": handle }));
                    } else {
                        reply_json(&reply_c, &ctx_c, &serde_json::json!({ "handle": null }));
                    }
                } else {
                    reply_json(&reply_c, &ctx_c, &serde_json::json!({ "handle": null }));
                }
            });
            dialog.show();
        }

        "openFile" | "convertFile" => {
            let handle = msg.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = match shell.handles.borrow().resolve(handle) {
                Some(p) => p,
                None => {
                    reply_err(reply, format!("{}: unknown handle: {handle}", msg.cmd));
                    return;
                }
            };
            if msg.cmd == "openFile" {
                *shell.current_path.borrow_mut() = Some(path.clone());
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let reply_c = reply.clone();
            let ctx_c = ctx.clone();
            x2t_convert_async(shell, path, fmt::BIN, None, move |result| match result {
                Ok(bytes) => {
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    reply_json(
                        &reply_c,
                        &ctx_c,
                        &serde_json::json!({ "data": b64, "images": {}, "name": name }),
                    );
                }
                Err(e) => reply_err(&reply_c, e),
            });
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
            let tmp_bin =
                std::env::temp_dir().join(format!("{}.bin", temp_stem("save")));
            if let Err(e) = std::fs::write(&tmp_bin, &bin) {
                reply_err(reply, format!("write temp Editor.bin: {e}"));
                return;
            }

            // Save As (or no current path): show the dialog INLINE and
            // reply from its response. Do NOT bounce an error to the shim.
            let needs_dialog = save_as || shell.current_path.borrow().is_none();
            if needs_dialog {
                if shell.active_dialog.borrow().is_some() {
                    let _ = std::fs::remove_file(&tmp_bin);
                    reply_err(reply, "saveFile: dialog already open".to_string());
                    return;
                }
                let window = shell.window.borrow().clone();
                let dialog = FileChooserNative::new(
                    Some("Save document"),
                    window.as_ref(),
                    FileChooserAction::Save,
                    Some("_Save"),
                    Some("_Cancel"),
                );
                dialog.set_current_name("Untitled.docx");
                *shell.active_dialog.borrow_mut() = Some(dialog.clone());
                let shell_c = shell.clone();
                let reply_c = reply.clone();
                let ctx_c = ctx.clone();
                let tmp_bin_c = tmp_bin.clone();
                dialog.connect_response(move |d, resp| {
                    *shell_c.active_dialog.borrow_mut() = None;
                    if resp != ResponseType::Accept {
                        let _ = std::fs::remove_file(&tmp_bin_c);
                        // Cancel is not an error: the shim checks
                        // `cancelled` and does not flag a save failure.
                        reply_json(
                            &reply_c,
                            &ctx_c,
                            &serde_json::json!({ "ok": false, "cancelled": true }),
                        );
                        return;
                    }
                    let Some(path) = d.file().and_then(|f| f.path()) else {
                        let _ = std::fs::remove_file(&tmp_bin_c);
                        reply_err(&reply_c, "saveFile: no path chosen".to_string());
                        return;
                    };
                    *shell_c.current_path.borrow_mut() = Some(path.clone());
                    let fmt_to = format_for_path(&path);
                    let reply_c2 = reply_c.clone();
                    let ctx_c2 = ctx_c.clone();
                    let shell_c2 = shell_c.clone();
                    x2t_convert_async(&shell_c2.clone(), tmp_bin_c.clone(), fmt_to, Some(fmt::BIN), move |result| match result {
                        Ok(_) => {
                            reply_json(&reply_c2, &ctx_c2, &serde_json::json!({ "ok": true }));
                            maybe_destroy_after_save(&shell_c2);
                        }
                        Err(e) => reply_err(&reply_c2, e),
                    });
                });
                dialog.show();
                return;
            }

            // Normal save to the current path.
            let path = shell.current_path.borrow().clone().expect("current_path");
            let fmt_to = format_for_path(&path);
            let reply_c = reply.clone();
            let ctx_c = ctx.clone();
            let shell_c = shell.clone();
            x2t_convert_async(&shell_c.clone(), tmp_bin, fmt_to, Some(fmt::BIN), move |result| match result {
                Ok(_) => {
                    reply_json(&reply_c, &ctx_c, &serde_json::json!({ "ok": true }));
                    maybe_destroy_after_save(&shell_c);
                }
                Err(e) => reply_err(&reply_c, e),
            });
        }

        "createNew" => {
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

        // Save-on-close: BLOCK the close until the save completes.
        // Returning Proceed immediately would tear the window down
        // before the async save (worker thread + dialog) finishes —
        // a data-loss path. Instead: trigger the save, return Stop,
        // and let maybe_destroy_after_save() destroy the window once
        // the save reply lands. A 10s watchdog destroys anyway so a
        // wedged save cannot hang the close forever.
        let shell_close = shell.clone();
        window.connect_close_request(move |win| {
            // Already waiting on a save: allow this close through.
            // (Destroying the window does not re-fire close-request,
            // but the guard makes the state machine explicit.)
            if *shell_close.close_pending.borrow() {
                return gtk4::glib::Propagation::Proceed;
            }
            if shell_close.current_path.borrow().is_some() {
                *shell_close.close_pending.borrow_mut() = true;
                if let Some(child) = win.child() {
                    if let Ok(view) = child.downcast::<WebView>() {
                        view.evaluate_javascript(
                            "window.AscDesktopEditor && window.AscDesktopEditor.LocalFileSave && window.AscDesktopEditor.LocalFileSave('', null, null, 0, null);",
                            None,
                            None,
                            None::<&gtk4::gio::Cancellable>,
                            |_| {},
                        );
                    }
                }
                // Watchdog: destroy after 10s even if the save never lands.
                let shell_watch = shell_close.clone();
                gtk4::glib::timeout_add_local_once(
                    std::time::Duration::from_secs(10),
                    move || {
                        if let Some(w) = shell_watch.window.borrow().as_ref() {
                            w.destroy();
                        }
                    },
                );
                return gtk4::glib::Propagation::Stop;
            }
            gtk4::glib::Propagation::Proceed
        });

        // WebContext with the joffice:// scheme + CORS. Register the
        // scheme BEFORE constructing the WebView.
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

        // NAVIGATION POLICY: allow the editor origin + the schemes the
        // editor itself uses (blob/data/about:blank for its iframes).
        // External http/https/mailto links open in the system browser.
        // Everything else: let WebKit decide (don't break internals).
        view.connect_decide_policy(|_view, decision, decision_type| {
            if decision_type != PolicyDecisionType::NavigationAction {
                return false;
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
            if uri.starts_with(EDITOR_ORIGIN)
                || uri.starts_with("blob:")
                || uri.starts_with("data:")
                || uri == "about:blank"
                || uri.is_empty()
            {
                return false; // allow editor + its own frames
            }
            if uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("mailto:") {
                let _ = AppInfo::launch_default_for_uri(&uri, None::<&gtk4::gio::AppLaunchContext>);
                decision.ignore();
                return true;
            }
            false
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
