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
use std::time::Instant;
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
/// The desktop font scheme: sdkjs's desktop font loader fetches
/// "ascdesktop://fonts/" + font-path (Local/common.js
/// CFontFileLoader.LoadFontAsync). The Qt shell serves it from the
/// app fonts; we serve it from the assembled fonts root.
const FONT_SCHEME: &str = "ascdesktop";

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
// Build stamp the web-apps deploy build normally injects into the
// page (About dialog version label reads the bare global). The
// shell is the build authority for its runtime: stamp it here.
window.__ABOUT_BUILD_INFO__ = ' (jOffice shell; web-apps f50e7e32, sdkjs 0cde84e8)';
window.__ABOUT_LEGAL__ = 'Euro-Office Desktop Editors: free software under the GNU Affero General Public License v3. Native shell: jOffice (Rust, GTK4, WebKitGTK), AGPL-3.0.';
// Native-environment bag the Qt/CEF shell injects per page as
// window.RendererProcessVariable (Desktop.js captures it into its
// module-scoped `nativevars`). Its theme readers are unguarded
// (systemThemeType does nativevars.theme && ...), so a missing bag
// turned into the "save a backup copy" warning dialog (cycle 2,
// boot16). Values describe this shell honestly: Linux, no local
// theme packs, no default printer, English keyboard. The
// window.nativevars alias costs nothing and covers any other
// reader of the plainer name.
window.RendererProcessVariable = {
    os: 'linux',
    theme: {},
    localthemes: [],
    keyboard: { langs: ['en'] },
    rtl: false,
    helpUrl: '',
    defaultPrinterName: ''
};
window.nativevars = window.RendererProcessVariable;
window.__jofficeErrors = [];
window.addEventListener('error', function (e) {
  window.__jofficeErrors.push('error: ' + e.message + ' @ ' + e.filename + ':' + e.lineno);
  if (e.error && e.error.stack) {
    console.error('[jOffice-stack] ' + e.error.stack);
  }
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

/// Reply a SAVE failure: log it, tell the shim (reply_err → the
/// shim signals EndSave(1)), AND show the user a native alert —
/// round 2 §4: x2t/save errors are surfaced, never silent.
/// EndSave(1) alone shows nothing in this editor build (drive
/// F4: the failure was in the log, the UI sat quiet), so the
/// native dialog is the user-visible half.
fn reply_save_err(shell: &Rc<Shell>, reply: &ScriptMessageReply, msg: String) {
    eprintln!("joffice: SAVE-ERROR {msg}");
    if let Some(win) = shell.window.borrow().as_ref() {
        let err_win = gtk4::Window::builder()
            .title("Save failed")
            .transient_for(win)
            .modal(false)
            .default_width(440)
            .build();
        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        vbox.set_margin_top(16);
        vbox.set_margin_bottom(16);
        vbox.set_margin_start(16);
        vbox.set_margin_end(16);
        let label = gtk4::Label::new(Some(&msg));
        label.set_wrap(true);
        label.set_halign(gtk4::Align::Start);
        let button = gtk4::Button::with_label("OK");
        button.set_halign(gtk4::Align::End);
        let win_c = err_win.clone();
        button.connect_clicked(move |_| win_c.close());
        vbox.append(&label);
        vbox.append(&button);
        err_win.set_child(Some(&vbox));
        err_win.present();
    }
    reply_err(reply, msg);
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

/// A document file's identity stamp at open time (marathon 2,
/// Kagi round 2 §4): before a save renames over the file, the
/// stamp is re-taken and compared — a mismatch means someone
/// else changed the document under us, and the save refuses
/// loudly instead of silently overwriting their work.
#[derive(Clone, PartialEq, Debug)]
struct FileStamp {
    size: u64,
    mtime: Option<std::time::SystemTime>,
    inode: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let md = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        md.ino()
    };
    #[cfg(not(unix))]
    let inode = 0;
    Some(FileStamp {
        size: md.len(),
        mtime: md.modified().ok(),
        inode,
    })
}

/// FNV-1a 64-bit — a build-identity hash for the runtime manifest
/// log (not cryptographic; it answers "which bytes are these").
fn fnv_file(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in &data {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(format!("{hash:016x}"))
}

/// Does this font family name claim CJK coverage? (Kagi round 2
/// §5: the fallback is chosen by the coverage the real font
/// claims.) The principled source is the ranges table inside
/// font_selection.bin; parsing that binary format is out of
/// scope for this cycle, so the decision uses the family name
/// against the CJK families plausibly present in a generated
/// set. Limitation recorded in marathon2/CYCLES.md.
fn is_cjk_family(name: &str) -> bool {
    const CJK: &[&str] = &[
        "nanum", "noto sans cjk", "noto serif cjk", "source han",
        "malgun", "gulim", "batang", "dotum", "gungsuh",
        "ms gothic", "ms mincho", "yu gothic", "yu mincho", "meiryo",
        "simsun", "simhei", "fangsong", "kaiti",
        "microsoft yahei", "microsoft jhenghei", "pingfang",
        "hiragino", "wenquanyi", "undotum", "unbatang", "baekmuk",
        "ipagothic", "ipamincho", "kochi", "sazanami",
        "droid sans fallback", "noto sans kr", "noto sans jp",
        "noto sans sc", "noto sans tc", "noto sans hk",
        "apple sd gothic", "applegothic", "applemyungjo",
    ];
    let n = name.to_lowercase();
    CJK.iter().any(|c| n.contains(c))
}

/// The generated web AllFonts.js, parsed once at startup: the
/// file list the editor will request, the family name behind
/// each file index, and the served paths of the two designated
/// fallbacks (Noto Sans CJK for CJK claims, Liberation Sans
/// otherwise). Drives the scheme handler's FONT_MISS decision.
#[derive(Default)]
struct FontIndex {
    /// __fonts_files entries, in order (the loader's Ids).
    files: Vec<String>,
    /// file index -> family name (from __fonts_infos; an info
    /// entry references up to four face files of one family).
    name_by_file: HashMap<usize, String>,
    /// family name -> its REGULAR face's file index (the first
    /// value in the infos entry). The web file list is numbered
    /// ("014"), so the regular face cannot be found by filename
    /// — it must come from the entry structure. (Drive F1b:
    /// picking by iteration order served Liberation Sans BOLD
    /// as the fallback.)
    regular_by_name: HashMap<String, usize>,
    cjk_fallback: Option<String>,
    latin_fallback: Option<String>,
}

impl FontIndex {
    fn load(sdkjs_dir: Option<&Path>) -> FontIndex {
        let mut index = FontIndex::default();
        let Some(dir) = sdkjs_dir else { return index };
        let Ok(text) = std::fs::read_to_string(dir.join("common/AllFonts.js")) else {
            return index;
        };
        index.files = parse_js_string_array(&text, "__fonts_files");
        (index.name_by_file, index.regular_by_name) = parse_fonts_infos(&text);
        // Fallbacks are the families' REGULAR faces.
        index.latin_fallback = index
            .regular_by_name
            .get("Liberation Sans")
            .and_then(|i| index.files.get(*i).cloned());
        index.cjk_fallback = index
            .regular_by_name
            .iter()
            .find(|(name, _)| name.starts_with("Noto Sans CJK"))
            .and_then(|(_, i)| index.files.get(*i).cloned())
            .or_else(|| index.latin_fallback.clone());
        index
    }

    fn name_for_path(&self, id: &str) -> Option<String> {
        let pos = self.files.iter().position(|f| f == id)?;
        self.name_by_file.get(&pos).cloned()
    }

    /// (served path, fallback family name) for a missing font, or
    /// None when no fallback exists in the generated set.
    fn fallback_for(&self, real_name: &str) -> Option<(String, String)> {
        if is_cjk_family(real_name) {
            if let Some(p) = &self.cjk_fallback {
                return Some((p.clone(), "Noto Sans CJK".to_string()));
            }
        }
        self.latin_fallback
            .as_ref()
            .map(|p| (p.clone(), "Liberation Sans".to_string()))
    }
}

/// Extract a `window["<key>"] = [ "a", "b", ... ];` string array
/// from AllFonts.js text. Tolerant scanner, not a JS parser —
/// the generator's output format is regular.
fn parse_js_string_array(text: &str, key: &str) -> Vec<String> {
    let Some(start) = text.find(key) else { return Vec::new() };
    let Some(open) = text[start..].find('[') else { return Vec::new() };
    let body_start = start + open + 1;
    let Some(close) = text[body_start..].find(']') else { return Vec::new() };
    let body = &text[body_start..body_start + close];
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escaped = false;
    for ch in body.chars() {
        if in_str {
            if escaped {
                cur.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                out.push(std::mem::take(&mut cur));
                in_str = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_str = true;
        }
    }
    out
}

/// Extract __fonts_infos entries: `["Name",i0,f0,i1,f1,...]`.
/// Every non-negative integer in an entry is a file index of
/// that family (regular/bold/italic/bold-italic faces) — map
/// them all to the family name. Returns (name_by_file,
/// regular_by_name): the first value of an entry is the
/// family's regular face.
fn parse_fonts_infos(text: &str) -> (HashMap<usize, String>, HashMap<String, usize>) {
    let mut map = HashMap::new();
    let mut regular = HashMap::new();
    let Some(start) = text.find("__fonts_infos") else { return (map, regular) };
    let Some(open) = text[start..].find('[') else { return (map, regular) };
    let mut rest = &text[start + open + 1..];
    loop {
        // Find the next entry start `["`.
        let Some(e) = rest.find("[\"") else { break };
        rest = &rest[e + 2..];
        let Some(q) = rest.find('"') else { break };
        let name = rest[..q].to_string();
        rest = &rest[q + 1..];
        let Some(close) = rest.find(']') else { break };
        let nums = &rest[..close];
        // Entries are (file-index, style-flag) pairs: only the
        // even-position VALUES are file indexes. (The token list
        // starts with an empty token from the comma after the
        // name — positions must be counted over parsed values,
        // not raw tokens; counting raw tokens inverts the pairs
        // and maps the flags. Caught by the cycle-1 F1 drive:
        // every name lookup missed.)
        let values: Vec<i64> = nums
            .split(',')
            .filter_map(|tok| tok.trim().parse::<i64>().ok())
            .collect();
        for (pos, n) in values.iter().enumerate() {
            if pos % 2 == 0 && *n >= 0 {
                map.insert(*n as usize, name.clone());
            }
        }
        if let Some(first) = values.first() {
            if *first >= 0 {
                regular.insert(name.clone(), *first as usize);
            }
        }
        rest = &rest[close + 1..];
        // Stop at the end of the infos array (a `]` at depth 0
        // appears as "]\n" followed by a non-entry); the loop's
        // entry search simply fails when entries run out.
        if rest.trim_start().starts_with(']') {
            break;
        }
    }
    (map, regular)
}

/// Shared shell state.
struct Shell {
    /// Directory containing the web-apps checkout (editor JS), canonicalized.
    webapps_dir: PathBuf,
    /// Directory containing the sdkjs checkout, canonicalized. The
    /// editor pages reference ../../../../sdkjs/... — sdkjs is a
    /// SIBLING of web-apps in the real layout, served under the
    /// /sdkjs/ prefix of the same origin with its own containment
    /// root. None = not found (sdkjs requests will 404).
    sdkjs_dir: Option<PathBuf>,
    /// Directory with the core font files AllFonts.js names as
    /// "/core-fonts/<file>" (the deb installs it beside the editors).
    /// Served under the /core-fonts/ prefix with its own containment
    /// root. None = not found (font file requests will 404).
    core_fonts_dir: Option<PathBuf>,
    /// Root the ascdesktop://fonts/ handler serves from. Since
    /// marathon 2 this is the allfontsgen --output-web directory:
    /// the generated web AllFonts.js names exactly the files here.
    /// None = not found (font fetches 404).
    fonts_root: Option<PathBuf>,
    /// The ORIGINAL fonts root (the allfontsgen --input): x2t's
    /// m_sFontDir. None = font params omitted from conversions.
    fonts_src_dir: Option<PathBuf>,
    /// The generated native AllFonts.js for x2t (m_sAllFontsPath).
    /// font_selection.bin sits beside it (x2t finds it there).
    x2t_allfonts: Option<PathBuf>,
    /// Blank templates for the launcher's New buttons
    /// (converter/empty/<locale>/). None = New buttons disabled.
    templates_dir: Option<PathBuf>,
    /// Parsed generated web AllFonts.js (fallback decisions).
    font_index: FontIndex,
    /// Font substitutions served this session, in order:
    /// (real family name, fallback family name). Drives the
    /// banner and the save-time reminder (Kagi round 2 §5).
    substitutions: RefCell<Vec<(String, String)>>,
    /// The save-time substitution reminder has been shown.
    save_toasted: RefCell<bool>,
    /// The substitution banner widgets (set after window build).
    banner_revealer: RefCell<Option<gtk4::Revealer>>,
    banner_label: RefCell<Option<gtk4::Label>>,
    /// Identity stamp of the currently open document at open /
    /// last save (external-change check, §4).
    doc_stamp: RefCell<Option<(PathBuf, FileStamp)>>,
    /// Save sequence numbers (§4 overlapping saves): each saveFile
    /// takes the next number; a completion whose number is not
    /// newer than the last written one is superseded and skipped.
    save_seq: RefCell<u64>,
    save_written_seq: RefCell<u64>,
    /// When the launcher opens a NEW document: the suggested
    /// save-as name. The pendingFile handler then does NOT set
    /// current_path (first save prompts via the dialog).
    open_as_new: RefCell<Option<String>>,
    /// The WebView (set after construction; the close handler
    /// needs it to trigger save-on-close).
    view: RefCell<Option<WebView>>,
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
    fn new(webapps_dir: PathBuf, sdkjs_dir: Option<PathBuf>, pending_file: Option<PathBuf>) -> Rc<Self> {
        let webapps_dir = webapps_dir.canonicalize().unwrap_or(webapps_dir);
        let sdkjs_dir = sdkjs_dir.and_then(|d| d.canonicalize().ok());
        // core-fonts lookup: $JOFFICE_COREFONTS first, then the
        // sibling of web-apps (the deb layout).
        let core_fonts_dir = std::env::var_os("JOFFICE_COREFONTS")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(webapps_dir.join("../core-fonts")))
            .and_then(|d| d.canonicalize().ok());
        // fonts root lookup: $JOFFICE_FONTSROOT first, then the
        // sibling of web-apps. Marathon 2: the allfontsgen
        // --output-web directory (fonts-gen/web/fonts) — the
        // generated web AllFonts.js paths are relative to it.
        let fonts_root = std::env::var_os("JOFFICE_FONTSROOT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(webapps_dir.join("../fonts-gen/web/fonts")))
            .and_then(|d| d.canonicalize().ok());
        // x2t lookup: $JOFFICE_X2T first, then the packaged layout
        // (<exe-dir>/../converter/x2t, mirroring the upstream deb),
        // then system paths and the core build tree.
        let x2t = std::env::var_os("JOFFICE_X2T")
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|p| p.join("../converter/x2t")))
                    .filter(|p| p.exists())
            })
            .or_else(|| {
                ["/usr/local/bin/x2t", "/usr/bin/x2t", "./x2t", "../core/build/bin/x2t"]
                    .iter()
                    .map(PathBuf::from)
                    .find(|p| p.exists())
            });
        // fonts source root (x2t m_sFontDir): $JOFFICE_FONTSDIR,
        // else the sibling fonts-src.
        let fonts_src_dir = std::env::var_os("JOFFICE_FONTSDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(webapps_dir.join("../fonts-src")))
            .and_then(|d| d.canonicalize().ok());
        // native AllFonts.js for x2t: $JOFFICE_ALLFONTS, else the
        // sibling fonts-gen/x2t/AllFonts.js.
        let x2t_allfonts = std::env::var_os("JOFFICE_ALLFONTS")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(webapps_dir.join("../fonts-gen/x2t/AllFonts.js")))
            .filter(|p| p.exists());
        // Templates: <converter dir>/empty/en-US (the converter
        // ships blank new.docx/new.xlsx/new.pptx per locale).
        let templates_dir = x2t.as_ref().and_then(|x| {
            let d = x.parent()?.join("empty/en-US");
            if d.join("new.docx").exists() {
                Some(d)
            } else {
                None
            }
        });
        let font_index = FontIndex::load(sdkjs_dir.as_deref());

        let mut handles = HandleTable::new();
        // Register --file at startup so it is resolvable as a handle.
        if let Some(p) = &pending_file {
            handles.insert(p.clone());
        }
        let shell = Rc::new(Self {
            webapps_dir,
            sdkjs_dir,
            core_fonts_dir,
            fonts_root,
            fonts_src_dir,
            x2t_allfonts,
            templates_dir,
            font_index,
            substitutions: RefCell::new(Vec::new()),
            save_toasted: RefCell::new(false),
            banner_revealer: RefCell::new(None),
            banner_label: RefCell::new(None),
            doc_stamp: RefCell::new(None),
            save_seq: RefCell::new(0),
            save_written_seq: RefCell::new(0),
            open_as_new: RefCell::new(None),
            view: RefCell::new(None),
            x2t,
            pending_file: RefCell::new(pending_file),
            handles: Rc::new(RefCell::new(handles)),
            window: RefCell::new(None),
            current_path: RefCell::new(None),
            active_dialog: RefCell::new(None),
            close_pending: RefCell::new(false),
        });
        shell.log_manifest();
        shell
    }

    /// Runtime manifest at boot (Kagi round 1 instrumentation):
    /// which x2t, which font maps, how many fonts — so a drive
    /// log states the exact runtime it graded.
    fn log_manifest(&self) {
        let x2t_desc = match &self.x2t {
            Some(p) => format!(
                "{} fnv={}",
                p.display(),
                fnv_file(p).unwrap_or_else(|| "?".to_string())
            ),
            None => "NOT FOUND".to_string(),
        };
        eprintln!("joffice: MANIFEST x2t: {x2t_desc}");
        if let Some(dir) = &self.sdkjs_dir {
            let af = dir.join("common/AllFonts.js");
            eprintln!(
                "joffice: MANIFEST web AllFonts.js: {} fnv={} ({} files in generated index)",
                af.display(),
                fnv_file(&af).unwrap_or_else(|| "?".to_string()),
                self.font_index.files.len()
            );
        }
        if let Some(p) = &self.x2t_allfonts {
            eprintln!(
                "joffice: MANIFEST x2t AllFonts.js: {} fnv={}",
                p.display(),
                fnv_file(p).unwrap_or_else(|| "?".to_string())
            );
        }
        eprintln!(
            "joffice: MANIFEST fonts src: {:?}, fonts root (web): {:?}",
            self.fonts_src_dir, self.fonts_root
        );
        eprintln!(
            "joffice: MANIFEST font index: {} named files, cjk fallback: {:?}, latin fallback: {:?}",
            self.font_index.name_by_file.len(),
            self.font_index.cjk_fallback,
            self.font_index.latin_fallback
        );
    }

    /// Record that `path` is the open document and stamp it
    /// (external-change check + recents, §4).
    fn note_document_opened(&self, path: &Path) {
        *self.doc_stamp.borrow_mut() = file_stamp(path).map(|s| (path.to_path_buf(), s));
        let uri = gtk4::gio::File::for_path(path).uri();
        gtk4::RecentManager::default().add_item(&uri);
    }

    /// Before writing a save over `dest`: refuse if the file on
    /// disk no longer matches the stamp taken at open / last
    /// save. (Only checked when the stamp is for this path —
    /// a Save As target has no stamp and nothing to protect.)
    fn check_unchanged(&self, dest: &Path) -> Result<(), String> {
        let recorded = self.doc_stamp.borrow().clone();
        if let Some((stamped_path, stamp)) = recorded {
            if stamped_path == dest {
                match file_stamp(dest) {
                    Some(now) if now == stamp => Ok(()),
                    Some(_) => Err(format!(
                        "save refused: {} changed on disk since it was opened (another app or a sync client touched it). Save As to keep both copies.",
                        dest.display()
                    )),
                    None => Err(format!(
                        "save refused: {} disappeared from disk since it was opened.",
                        dest.display()
                    )),
                }
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    /// After a successful save write: refresh the stamp, and
    /// show the once-per-session substitution reminder (§5).
    fn note_save_completed(&self, dest: &Path) {
        *self.doc_stamp.borrow_mut() = file_stamp(dest).map(|s| (dest.to_path_buf(), s));
        let subs = self.substitutions.borrow();
        if !subs.is_empty() && !*self.save_toasted.borrow() {
            *self.save_toasted.borrow_mut() = true;
            drop(subs);
            self.refresh_banner(true);
        }
    }

    /// (Re)build the substitution banner text and reveal it.
    /// `save_reminder` adds the §5 save-time line.
    fn refresh_banner(&self, save_reminder: bool) {
        let subs = self.substitutions.borrow();
        if subs.is_empty() {
            return;
        }
        let list = subs
            .iter()
            .map(|(real, fb)| format!("{real} (shown as {fb})"))
            .collect::<Vec<_>>()
            .join(", ");
        drop(subs);
        let mut text = format!("Fonts not installed: {list}. Layout may differ.");
        if save_reminder {
            text.push_str(" Saved with substituted fonts — check layout before sharing.");
        }
        if let Some(label) = self.banner_label.borrow().as_ref() {
            label.set_text(&text);
        }
        if let Some(rev) = self.banner_revealer.borrow().as_ref() {
            rev.set_reveal_child(true);
        }
    }

    /// Record a font substitution (deduped) and show the banner.
    fn note_substitution(&self, real: &str, fallback: &str) {
        {
            let mut subs = self.substitutions.borrow_mut();
            let entry = (real.to_string(), fallback.to_string());
            if subs.contains(&entry) {
                return;
            }
            subs.push(entry);
        }
        self.refresh_banner(false);
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

/// Write bytes to `dest` as a document save (marathon 2, Kagi
/// round 2 §4 — the parts of save safety atomic rename alone
/// does NOT give you):
///   * Symlinks: the destination is canonicalized first, so the
///     rename lands on the real target instead of replacing the
///     link itself and leaving the target stale.
///   * The temp file is fsynced, then renamed, then the PARENT
///     DIRECTORY is fsynced — without the last step a power loss
///     can lose the rename itself.
///   * The existing file's permissions are copied onto the temp
///     file, so a save never silently changes the document's mode.
///   * The temp name is hidden and document-named
///     (.~<name>.joffice-tmp-…) so a crash leftover is neither
///     mistaken for a document nor eagerly uploaded by sync
///     tools; stale leftovers in the directory are swept.
/// Ok(()) is returned only after rename + directory fsync —
/// callers clear the editor's modified flag on our ok reply,
/// so ok must mean durable.
fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Canonicalize the target: an existing file resolves its
    // symlinks; a new file canonicalizes its parent instead.
    let dest_canon = match dest.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            let parent = dest.parent().unwrap_or_else(|| Path::new("."));
            let canon_parent = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            match dest.file_name() {
                Some(name) => canon_parent.join(name),
                None => dest.to_path_buf(),
            }
        }
    };
    let dir = dest_canon
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let name = dest_canon
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_string());

    // Sweep stale temp leftovers from crashed saves (>1 h old).
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            if fname.starts_with(".~") && fname.contains(".joffice-tmp-") {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| {
                        t.elapsed()
                            .map(|d| d > std::time::Duration::from_secs(3600))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    let tmp = dir.join(format!(
        ".~{name}.joffice-tmp-{}",
        temp_stem("save")
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // Preserve the replaced file's permissions.
        if let Ok(md) = std::fs::metadata(&dest_canon) {
            let _ = f.set_permissions(md.permissions());
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &dest_canon)?;
        // Fsync the parent directory so the rename is durable.
        let dir_file = std::fs::File::open(&dir)?;
        dir_file.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
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
    font_dir: Option<&Path>,
    allfonts_path: Option<&Path>,
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
    // Font wiring (marathon 2, Kagi round 2 §2): two separate
    // elements, read independently. m_sFontDir points at the
    // ORIGINAL fonts root (the allfontsgen --input), and
    // m_sAllFontsPath at the generated NATIVE AllFonts.js —
    // font_selection.bin sits beside it and x2t finds it there
    // (there is no selection element in the XML schema).
    if let Some(d) = font_dir {
        extra.push_str(&format!(
            "  <m_sFontDir>{}</m_sFontDir>\n",
            xml_escape(&d.display().to_string())
        ));
    } else {
        // Preserve the pre-marathon-2 shape when no font root is
        // configured: an empty element, x2t uses its defaults.
        extra.push_str("  <m_sFontDir />\n");
    }
    if let Some(p) = allfonts_path {
        extra.push_str(&format!(
            "  <m_sAllFontsPath>{}</m_sAllFontsPath>\n",
            xml_escape(&p.display().to_string())
        ));
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
    let font_dir = shell.fonts_src_dir.clone();
    let allfonts = shell.x2t_allfonts.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
    std::thread::spawn(move || {
        let result = x2t_convert_sync(
            &x2t,
            &from,
            format_to,
            format_from,
            font_dir.as_deref(),
            allfonts.as_deref(),
        );
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

/// Percent-encode a URL query value (unreserved set passes through).
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the editor entry URL. With a pending file, this is the
/// api/documents desktop bootstrap with the document described in
/// query parameters (the page builds a DocsAPI.DocEditor config
/// from them and iframes the matching editor app). The document
/// key is a stable FNV-1a hash of the canonical path: the same
/// file reopens as the same document. Without a file, fall back to
/// the bare editor app — a known-incomplete start state (the
/// desktop product's start window is a separate native surface
/// this shell does not have yet).
fn editor_entry_url(pending: Option<&PathBuf>) -> String {
    let Some(path) = pending else {
        return "joffice:///apps/documenteditor/main/index.html".to_string();
    };
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let doctype = match ext.as_str() {
        "xlsx" | "xls" | "ods" | "csv" | "tsv" => "cell",
        "pptx" | "ppt" | "odp" => "slide",
        "pdf" => "pdf",
        _ => "word",
    };
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in canon.to_string_lossy().bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!(
        "joffice:///apps/api/documents/index.html?type=desktop&doctype={doctype}&key=joffice-{hash:016x}&url=_offline_&title={}&filetype={}&mode=edit&lang=en",
        query_escape(&title),
        query_escape(&ext),
    )
}

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
        "otf" => "font/otf",
        "ttc" => "font/collection",
        _ => "application/octet-stream",
    }
}

/// Finish a scheme request with a full response carrying
/// `Access-Control-Allow-Origin: *`. Plain `finish()` sends no
/// CORS header, and WebKit then refuses cross-origin reads of
/// otherwise-200 responses (the ascdesktop font fetches failed
/// exactly that way in cycle 2: "Origin joffice:// is not allowed
/// by Access-Control-Allow-Origin. Status code: 200").
fn finish_ok(req: &URISchemeRequest, data: &[u8], mime: &str) {
    let stream = gtk4::gio::MemoryInputStream::from_bytes(&gtk4::glib::Bytes::from(data));
    let response = webkit6::URISchemeResponse::new(&stream, data.len() as i64);
    response.set_status(200, None);
    response.set_content_type(mime);
    let headers = webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
    headers.append("Access-Control-Allow-Origin", "*");
    response.set_http_headers(headers);
    req.finish_with_response(&response);
}

/// Serve a joffice:// URI from the web-apps directory, or from the
/// sdkjs sibling directory for /sdkjs/ paths (the editor pages
/// reference ../../../../sdkjs/... — a sibling tree, not part of
/// web-apps). Each tree has its own containment root.
/// Defenses: percent-decode, strip query/fragment, canonicalize and
/// assert the result stays under its tree's root (blocks ../../
/// traversal; canonicalize also resolves symlinks, so a symlink
/// pointing out of the tree fails containment and 404s).
fn handle_uri_scheme(shell: &Shell, req: &URISchemeRequest) {
    let uri = req.uri().unwrap_or_default();
    let rest = uri.strip_prefix(EDITOR_ORIGIN).unwrap_or("");
    // Strip query and fragment: ?v=2 and #hash are not filesystem.
    let path_part = rest.split(['?', '#']).next().unwrap_or("");
    // Percent-decode: %20 -> space, etc.
    let decoded = gtk4::glib::uri_unescape_string(path_part, None::<&str>)
        .map(|s| s.to_string())
        .unwrap_or_else(|| path_part.to_string());
    let rel = decoded.trim_start_matches('/');

    // Route /sdkjs/* to the sdkjs tree, /core-fonts/* to the
    // core-fonts tree; everything else to web-apps.
    let (root, sub) = match rel.strip_prefix("sdkjs/") {
        Some(sub) => match &shell.sdkjs_dir {
            Some(dir) => (dir.clone(), sub.to_string()),
            None => {
                eprintln!("joffice: scheme 404 (no sdkjs tree): {uri}");
                let mut err = gtk4::glib::Error::new(
                    gtk4::gio::IOErrorEnum::NotFound,
                    &format!("joffice: sdkjs tree not configured: {uri}"),
                );
                req.finish_error(&mut err);
                return;
            }
        },
        None => match rel.strip_prefix("core-fonts/") {
            Some(sub) => match &shell.core_fonts_dir {
                Some(dir) => (dir.clone(), sub.to_string()),
                None => {
                    eprintln!("joffice: scheme 404 (no core-fonts tree): {uri}");
                    let mut err = gtk4::glib::Error::new(
                        gtk4::gio::IOErrorEnum::NotFound,
                        &format!("joffice: core-fonts tree not configured: {uri}"),
                    );
                    req.finish_error(&mut err);
                    return;
                }
            },
            None => (shell.webapps_dir.clone(), rel.to_string()),
        },
    };
    let file_path = root.join(&sub);

    let file_path = if file_path.is_dir() {
        file_path.join("index.html")
    } else {
        file_path
    };

    // Canonicalize and verify containment. Nonexistent -> 404 (safe).
    match file_path.canonicalize() {
        Ok(canon) if canon.starts_with(&root) => match std::fs::read(&canon) {
            Ok(data) => {
                finish_ok(req, &data, content_type(&canon));
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
            // Misses are diagnostic gold (a wrong root or a
            // generated-file gap both look like this): log every 404.
            eprintln!("joffice: scheme 404: {uri}");
            let mut err = gtk4::glib::Error::new(
                gtk4::gio::IOErrorEnum::NotFound,
                &format!("joffice: not found: {uri}"),
            );
            req.finish_error(&mut err);
        }
    }
}

/// Finish a font-scheme request with an explicit status and
/// headers over a stream (marathon 2). Unlike finish_error, an
/// explicit response lets the loader's XHR complete with a real
/// status the JS wrap can act on, and lets us label substituted
/// fonts (X-JOffice-Font-Substitute) and true misses
/// (X-JOffice-Font: missing).
fn finish_font(
    req: &URISchemeRequest,
    status: u32,
    stream: &impl gtk4::gio::prelude::IsA<gtk4::gio::InputStream>,
    len: i64,
    mime: &str,
    extra_headers: &[(&str, &str)],
) {
    let response = webkit6::URISchemeResponse::new(stream, len);
    response.set_status(status, None);
    response.set_content_type(mime);
    let headers = webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
    headers.append("Access-Control-Allow-Origin", "*");
    // The font XHR is cross-origin (joffice:// page, ascdesktop://
    // fonts): without this, JS cannot read the labeling headers
    // back (getResponseHeader returns null) even though the
    // response carries them.
    headers.append(
        "Access-Control-Expose-Headers",
        "X-JOffice-Font-Substitute, X-JOffice-Font",
    );
    for (k, v) in extra_headers {
        headers.append(k, v);
    }
    response.set_http_headers(headers);
    req.finish_with_response(&response);
}

fn finish_font_empty(req: &URISchemeRequest, status: u32, extra_headers: &[(&str, &str)]) {
    let stream = gtk4::gio::MemoryInputStream::from_bytes(&gtk4::glib::Bytes::from(&[][..]));
    finish_font(req, status, &stream, 0, "application/octet-stream", extra_headers);
}

/// Serve a font file as a GFileInputStream hand-off: WebKit
/// pulls the bytes itself instead of the handler reading the
/// whole file on the main thread (Kagi round 2 §6 — synchronous
/// whole-file reads in a custom-scheme handler serialize every
/// font request and freeze the UI). Returns false if the file
/// cannot be opened.
fn serve_font_file(
    req: &URISchemeRequest,
    canon: &Path,
    extra_headers: &[(&str, &str)],
) -> bool {
    let file = gtk4::gio::File::for_path(canon);
    match file.read(None::<&gtk4::gio::Cancellable>) {
        Ok(stream) => {
            let len = std::fs::metadata(canon).map(|m| m.len() as i64).unwrap_or(-1);
            finish_font(req, 200, &stream, len, content_type(canon), extra_headers);
            true
        }
        Err(_) => false,
    }
}

/// Serve ascdesktop://fonts/<path> from the fonts root (the
/// allfontsgen --output-web directory). The font loader's Ids
/// are the paths the generated web AllFonts.js names.
///
/// Marathon 2 (Kagi round 2 §5/§6/§7): Rust decides missing vs
/// slow, and a miss is answered IMMEDIATELY and LABELED — never
/// left hanging:
///   * file present → stream it (ledger: FONT-REQ result=ok).
///   * file missing → serve the designated fallback (chosen by
///     the coverage the real font claims: CJK → Noto Sans CJK,
///     else Liberation Sans) with an X-JOffice-Font-Substitute
///     header naming real→fallback; log FONT_MISS; record the
///     substitution for the banner. The document loads with a
///     stand-in instead of freezing at 53% forever.
///   * no fallback → an explicit 404 with X-JOffice-Font:
///     missing; the JS wrap treats it as a final failure and
///     lets the load proceed without the font.
/// Every request is ledger-logged with its result and the
/// hand-off latency, for comparison against the JS ledger.
/// Resolve a font id to a servable file. Ids come in three
/// shapes, one per AllFonts.js layout: numeric (generated web
/// layout — an index whose file entry is itself the name),
/// root-relative paths (the deb's layout), and absolute paths
/// (the generated desktop layout: the editor-facing AllFonts.js
/// is the path-keyed one — its selection bin records are
/// path-keyed and the desktop font manager joins bin against
/// __fonts_files by path; numeric entries break substitution
/// lookups such as Calibri→Carlito — see marathon2/CYCLES.md).
/// Absolute ids are only served from inside the fonts root or
/// the fonts source dir, never arbitrary paths.
fn resolve_font_file(shell: &Shell, id: &str) -> Option<PathBuf> {
    let root = shell.fonts_root.as_ref()?;
    let mut candidates = vec![root.join(id)];
    if id.starts_with('/') {
        candidates.push(PathBuf::from(id));
    } else {
        candidates.push(PathBuf::from(format!("/{id}")));
    }
    for cand in candidates {
        if let Ok(canon) = cand.canonicalize() {
            if !canon.is_file() {
                continue;
            }
            let in_root = canon.starts_with(root);
            let in_src = shell
                .fonts_src_dir
                .as_ref()
                .map(|s| canon.starts_with(s))
                .unwrap_or(false);
            if in_root || in_src {
                return Some(canon);
            }
        }
    }
    None
}

fn handle_font_scheme(shell: &Shell, req: &URISchemeRequest) {
    let t0 = Instant::now();
    let uri = req.uri().unwrap_or_default();
    let Some(root) = &shell.fonts_root else {
        eprintln!("joffice: FONT-REQ uri={uri} result=no-fonts-root");
        finish_font_empty(req, 404, &[("X-JOffice-Font", "missing")]);
        return;
    };
    let rest = uri.strip_prefix("ascdesktop://").unwrap_or("");
    // Host must be "fonts"; the remainder is the font path.
    let Some((host, path)) = rest.split_once('/') else {
        finish_font_empty(req, 404, &[("X-JOffice-Font", "missing")]);
        return;
    };
    if host != "fonts" {
        finish_font_empty(req, 404, &[("X-JOffice-Font", "missing")]);
        return;
    }
    let path_part = path.split(['?', '#']).next().unwrap_or("");
    let decoded = gtk4::glib::uri_unescape_string(path_part, None::<&str>)
        .map(|s| s.to_string())
        .unwrap_or_else(|| path_part.to_string());
    let id = decoded.trim_start_matches('/').to_string();
    if let Some(canon) = resolve_font_file(shell, &id) {
        let bytes = std::fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);
        if serve_font_file(req, &canon, &[]) {
            eprintln!(
                "joffice: FONT-REQ id={id} result=ok bytes={bytes} ms={}",
                t0.elapsed().as_millis()
            );
            return;
        }
    }
    // Missing. Decide the labeled answer.
    let real_name = shell
        .font_index
        .name_for_path(&id)
        .or_else(|| shell.font_index.name_for_path(&format!("/{id}")))
        .unwrap_or_else(|| id.clone());
    if let Some((fb_path, fb_name)) = shell.font_index.fallback_for(&real_name) {
        if let Some(canon) = resolve_font_file(shell, fb_path.trim_start_matches('/')) {
            {
                // ASCII only in the header value: libsoup drops
                // or mangles non-ASCII header bytes, and drive
                // F1b showed the JS side then never sees the
                // label. "A->B" it is.
                let header = format!("{real_name}->{fb_name}");
                if serve_font_file(req, &canon, &[("X-JOffice-Font-Substitute", &header)]) {
                    eprintln!(
                        "joffice: FONT_MISS id={id} real={real_name} fallback={fb_name} ms={}",
                        t0.elapsed().as_millis()
                    );
                    eprintln!(
                        "joffice: FONT-REQ id={id} result=substitute fallback={fb_name} ms={}",
                        t0.elapsed().as_millis()
                    );
                    shell.note_substitution(&real_name, &fb_name);
                    return;
                }
            }
        }
    }
    eprintln!(
        "joffice: FONT-REQ id={id} result=missing real={real_name} ms={}",
        t0.elapsed().as_millis()
    );
    finish_font_empty(req, 404, &[("X-JOffice-Font", "missing")]);
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

/// Sanity-check converted output before it replaces a document
/// (marathon 2, Kagi round 2 §4 "the wrong output gets written"):
/// for the zip-family formats the output must be a zip, and for
/// OOXML it must carry [Content_Types].xml. A converter that
/// failed halfway must never hand us bytes we then rename over
/// the user's document.
fn save_output_sane(dest: &Path, bytes: &[u8]) -> bool {
    let ext = dest
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" => {
            if !bytes.starts_with(b"PK\x03\x04") {
                return false;
            }
            if matches!(ext.as_str(), "docx" | "xlsx" | "pptx") {
                // Zip local headers store entry names in plain
                // text; [Content_Types].xml must be among them.
                let needle: &[u8] = b"[Content_Types].xml";
                if !bytes.windows(needle.len()).any(|w| w == needle) {
                    return false;
                }
            }
            true
        }
        _ => !bytes.is_empty(),
    }
}

/// Shared tail of every save (marathon 2 §4): sanity check →
/// external-change check → sequence guard → atomic write →
/// stamp refresh. Returns Ok(true) when bytes were written,
/// Ok(false) when a newer save already landed (this one is
/// superseded; replying ok is honest — the newer content is
/// what's on disk). Err surfaces to the user via the shim.
fn write_save_checked(
    shell: &Shell,
    dest: &Path,
    bytes: &[u8],
    seq: u64,
) -> Result<bool, String> {
    if !save_output_sane(dest, bytes) {
        return Err(format!(
            "save refused: the converter output for {} failed the sanity check (not a valid document package). Your original file was not touched.",
            dest.display()
        ));
    }
    shell.check_unchanged(dest)?;
    if seq <= *shell.save_written_seq.borrow() {
        eprintln!(
            "joffice: save seq={seq} superseded by seq={} — skipped",
            shell.save_written_seq.borrow()
        );
        return Ok(false);
    }
    write_atomic(dest, bytes).map_err(|e| format!("save write: {e}"))?;
    *shell.save_written_seq.borrow_mut() = seq;
    shell.note_save_completed(dest);
    Ok(true)
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
    // view.uri() is the top-frame URI. Since cycle 2 the scripts
    // inject into ALL frames (the editor app runs in an iframe
    // under the api/documents entry page, mirroring how CEF exposes
    // the native object), so any frame of OUR origin can reach the
    // handler. The origin itself is the trust boundary; per-frame
    // attribution is a known limitation, documented.
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
                    // The pending file IS the document being opened:
                    // it becomes the save target. (Cycle 3: without
                    // this, current_path stayed None for --file
                    // opens — convertFile does not set it — and every
                    // Ctrl+S fell into the Save As dialog branch.)
                    // Exception (marathon 2): a launcher NEW document
                    // opens from a template copy in a temp dir —
                    // that copy must NOT become the save target, or
                    // the first save would silently land in /tmp.
                    // open_as_new marks it: no current_path (first
                    // save prompts), no stamp, no recents entry.
                    if shell.open_as_new.borrow().is_some() {
                        *shell.open_as_new.borrow_mut() = None;
                    } else {
                        *shell.current_path.borrow_mut() = Some(p.clone());
                        shell.note_document_opened(&p);
                    }
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
                shell.note_document_opened(&path);
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
            // Marathon 2 (§4): every save takes a sequence number;
            // completions write through write_save_checked, which
            // refuses stale, insane, or externally-conflicted saves.
            let seq = {
                let mut s = shell.save_seq.borrow_mut();
                *s += 1;
                *s
            };
            let b64 = msg.args.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let save_as = msg.args.get("saveAs").and_then(|v| v.as_bool()).unwrap_or(false);
            use base64::Engine;
            let bin = match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => {
                    reply_save_err(&shell, reply, format!("saveFile: bad base64: {e}"));
                    return;
                }
            };
            let tmp_bin =
                std::env::temp_dir().join(format!("{}.bin", temp_stem("save")));
            if let Err(e) = std::fs::write(&tmp_bin, &bin) {
                reply_save_err(&shell, reply, format!("write temp Editor.bin: {e}"));
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
                    let dest = path.clone();
                    x2t_convert_async(&shell_c2.clone(), tmp_bin_c.clone(), fmt_to, Some(fmt::BIN), move |result| match result {
                        Ok(bytes) => {
                            // Cycle 3 fix: the converter returns the
                            // output BYTES — write them to the target
                            // path. The October code replied ok and
                            // discarded them: save was a silent no-op.
                            // Marathon 2: the write goes through the
                            // checked tail (sanity, external-change,
                            // sequence) before it may land.
                            match write_save_checked(&shell_c2, &dest, &bytes, seq) {
                                Ok(_) => {
                                    reply_json(&reply_c2, &ctx_c2, &serde_json::json!({ "ok": true }));
                                    maybe_destroy_after_save(&shell_c2);
                                }
                                Err(e) => reply_save_err(&shell_c2, &reply_c2, e),
                            }
                        }
                        Err(e) => reply_save_err(&shell_c2, &reply_c2, e),
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
            let dest = path.clone();
            x2t_convert_async(&shell_c.clone(), tmp_bin, fmt_to, Some(fmt::BIN), move |result| match result {
                Ok(bytes) => {
                    // Write the converted bytes to the document
                    // path (see the dialog branch: this write is
                    // the actual save; October discarded the bytes).
                    // Marathon 2: through the checked tail first.
                    match write_save_checked(&shell_c, &dest, &bytes, seq) {
                        Ok(_) => {
                            reply_json(&reply_c, &ctx_c, &serde_json::json!({ "ok": true }));
                            maybe_destroy_after_save(&shell_c);
                        }
                        Err(e) => reply_save_err(&shell_c, &reply_c, e),
                    }
                }
                Err(e) => reply_save_err(&shell_c, &reply_c, e),
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
// Window geometry (marathon 2, Kagi round 2 §3)
//
// Persist width, height and maximized ONLY — never x/y: on
// Wayland a client cannot position its own window, and
// pretending otherwise stores fiction. Saved on close-request
// from the DEFAULT size (what the user actually chose, not the
// transient allocation), restored before present().
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct WindowGeometry {
    width: i32,
    height: i32,
    maximized: bool,
}

fn geometry_path() -> PathBuf {
    gtk4::glib::user_config_dir()
        .join("joffice")
        .join("window.json")
}

fn load_geometry() -> Option<WindowGeometry> {
    let text = std::fs::read_to_string(geometry_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_geometry(win: &ApplicationWindow) {
    let geo = WindowGeometry {
        width: win.default_width(),
        height: win.default_height(),
        maximized: win.is_maximized(),
    };
    let path = geometry_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&geo) {
        let _ = std::fs::write(&path, text);
    }
}

/// Recent documents for the launcher: GtkRecentManager items
/// (the bridge's note_document_opened writes opens into it),
/// filtered to document files that still exist locally.
fn recent_documents(limit: usize) -> Vec<(String, PathBuf)> {
    const EXTS: &[&str] = &[
        "docx", "xlsx", "pptx", "pdf", "odt", "ods", "odp", "doc", "xls", "ppt", "rtf", "txt",
        "csv",
    ];
    let mgr = gtk4::RecentManager::default();
    let mut out = Vec::new();
    for info in mgr.items() {
        if out.len() >= limit {
            break;
        }
        if !info.exists() {
            continue;
        }
        let uri = info.uri();
        let Some(path) = gtk4::gio::File::for_uri(&uri).path() else {
            continue;
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !EXTS.contains(&ext.as_str()) {
            continue;
        }
        out.push((info.display_name().to_string(), path));
    }
    out
}

/// Create a working copy of a blank template for a launcher
/// New button. Returns (copy path, suggested save-as name).
/// The copy lives in a per-process temp dir; it is the
/// conversion source for opening, never the save target.
fn make_new_document(shell: &Shell, ext: &str) -> Option<(PathBuf, String)> {
    let templates = shell.templates_dir.as_ref()?;
    let src = templates.join(format!("new.{ext}"));
    let dir = std::env::temp_dir().join(format!("joffice-new-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!("Untitled.{ext}"));
    std::fs::copy(&src, &dest).ok()?;
    Some((dest, format!("Untitled.{ext}")))
}

/// The native launcher shown on a no-args start (marathon 2,
/// Kagi round 2 §3): Open via the native file dialog, New from
/// the converter's blank templates, and recents from
/// GtkRecentManager. Every path funnels into open_document,
/// which loads the editor entry URL for the chosen file.
fn build_launcher(
    shell: &Rc<Shell>,
    window: &ApplicationWindow,
    open_document: &Rc<dyn Fn(PathBuf, Option<String>)>,
) -> gtk4::Box {
    let bx = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    bx.set_margin_start(28);
    bx.set_margin_end(28);
    bx.set_margin_top(24);
    bx.set_margin_bottom(24);

    let title = gtk4::Label::new(None);
    title.set_markup("<b><big>jOffice</big></b>");
    title.set_halign(gtk4::Align::Start);
    bx.append(&title);

    // Open…
    let open_btn = gtk4::Button::with_label("Open…");
    open_btn.set_halign(gtk4::Align::Start);
    {
        let shell_open = shell.clone();
        let window_open = window.clone();
        let od_open = open_document.clone();
        open_btn.connect_clicked(move |_| {
            if shell_open.active_dialog.borrow().is_some() {
                return;
            }
            let dialog = FileChooserNative::new(
                Some("Open document"),
                Some(&window_open),
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
            *shell_open.active_dialog.borrow_mut() = Some(dialog.clone());
            let shell_c = shell_open.clone();
            let od_c = od_open.clone();
            dialog.connect_response(move |d, resp| {
                *shell_c.active_dialog.borrow_mut() = None;
                if resp == ResponseType::Accept {
                    if let Some(path) = d.file().and_then(|f| f.path()) {
                        od_c(path, None);
                    }
                }
            });
            dialog.show();
        });
    }
    bx.append(&open_btn);

    // New from template.
    for (label, ext) in [
        ("New Document", "docx"),
        ("New Spreadsheet", "xlsx"),
        ("New Presentation", "pptx"),
    ] {
        let btn = gtk4::Button::with_label(label);
        btn.set_halign(gtk4::Align::Start);
        btn.set_sensitive(shell.templates_dir.is_some());
        let shell_c = shell.clone();
        let od_c = open_document.clone();
        btn.connect_clicked(move |_| match make_new_document(&shell_c, ext) {
            Some((path, name)) => od_c(path, Some(name)),
            None => eprintln!("joffice: launcher: no template available for {ext}"),
        });
        bx.append(&btn);
    }

    // Recents.
    let rec_label = gtk4::Label::new(None);
    rec_label.set_markup("<b>Recent documents</b>");
    rec_label.set_halign(gtk4::Align::Start);
    rec_label.set_margin_top(10);
    bx.append(&rec_label);
    let recents = recent_documents(10);
    if recents.is_empty() {
        let none = gtk4::Label::new(Some("No recent documents"));
        none.set_halign(gtk4::Align::Start);
        bx.append(&none);
    } else {
        for (name, path) in recents {
            let btn = gtk4::Button::with_label(&name);
            btn.set_halign(gtk4::Align::Start);
            let od_c = open_document.clone();
            btn.connect_clicked(move |_| od_c(path.clone(), None));
            bx.append(&btn);
        }
    }
    bx
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
    // Test-drive support, env-driven because GtkApplication rejects
    // unknown CLI options before our parsing runs: with
    // JOFFICE_SNAPSHOT=<path> set (and JOFFICE_SNAPSHOT_AFTER=<secs>,
    // default 20), the shell snapshots the WebView's rendered page
    // to the PNG path and exits. A page snapshot bypasses the
    // compositor — and any lock screen over it — and proves what the
    // EDITOR rendered, which is the thing being graded.
    let snapshot_path: Option<PathBuf> = std::env::var_os("JOFFICE_SNAPSHOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let snapshot_after_secs: u64 = std::env::var("JOFFICE_SNAPSHOT_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    // Resolve the web-apps dir: $JOFFICE_WEBAPPS, else
    // <exe-dir>/../web-apps, or ./web-apps.
    let webapps_dir = std::env::var_os("JOFFICE_WEBAPPS")
        .map(PathBuf::from)
        .filter(|p| p.join("apps/documenteditor/main/index.html").exists())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|p| p.to_path_buf()))
                .map(|exe_dir| exe_dir.join("../web-apps"))
                .filter(|p| p.join("apps/documenteditor/main/index.html").exists())
        })
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

    // Resolve the sdkjs dir (sibling of web-apps): $JOFFICE_SDKJS,
    // else <web-apps>/../sdkjs.
    let sdkjs_dir = std::env::var_os("JOFFICE_SDKJS")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            let p = webapps_dir.join("../sdkjs");
            if p.is_dir() {
                Some(p)
            } else {
                None
            }
        });

    let shell = Shell::new(webapps_dir, sdkjs_dir, pending_file);

    let app = Application::builder()
        .application_id("io.nowlin.jOffice")
        .build();

    let shell_clone = shell.clone();
    app.connect_activate(move |app| {
        let shell = shell_clone.clone();

        // Geometry restore (§3): size from the saved geometry,
        // maximized applied just before present() below.
        let saved_geo = load_geometry();
        let (geo_w, geo_h) = saved_geo
            .as_ref()
            .map(|g| (g.width, g.height))
            .unwrap_or((1280, 900));
        let window = ApplicationWindow::builder()
            .application(app)
            .title("jOffice")
            .default_width(geo_w)
            .default_height(geo_h)
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
            // Geometry is saved on EVERY close-request (§3),
            // before any save dance below.
            save_geometry(win);
            // Already waiting on a save: allow this close through.
            // (Destroying the window does not re-fire close-request,
            // but the guard makes the state machine explicit.)
            if *shell_close.close_pending.borrow() {
                return gtk4::glib::Propagation::Proceed;
            }
            if shell_close.current_path.borrow().is_some() {
                *shell_close.close_pending.borrow_mut() = true;
                if let Some(view) = shell_close.view.borrow().as_ref() {
                    view.evaluate_javascript(
                        "window.AscDesktopEditor && window.AscDesktopEditor.LocalFileSave && window.AscDesktopEditor.LocalFileSave('', null, null, 0, null);",
                        None,
                        None,
                        None::<&gtk4::gio::Cancellable>,
                        |_| {},
                    );
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

        // The desktop font scheme. CORS-enabled + secure so the
        // editor page's XHR loader may read it cross-origin; NOT
        // display-isolated (that would block the fetch entirely).
        security.register_uri_scheme_as_cors_enabled(FONT_SCHEME);
        security.register_uri_scheme_as_secure(FONT_SCHEME);
        let shell_fonts = shell.clone();
        context.register_uri_scheme(FONT_SCHEME, move |req| {
            handle_font_scheme(&shell_fonts, req);
        });

        // WebView with JS bridge + shim, injected into TOP frame only.
        let manager = UserContentManager::new();
        manager.register_script_message_handler_with_reply("joffice", None);

        let bootstrap = UserScript::new(
            BOOTSTRAP_JS,
            UserContentInjectedFrames::AllFrames,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        manager.add_script(&bootstrap);

        let shim = UserScript::new(
            SHIM_JS,
            UserContentInjectedFrames::AllFrames,
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

        // Snapshot mode (test drives): capture the rendered page
        // after the delay, write the PNG, and exit.
        if let Some(snap_path) = snapshot_path.clone() {
            let view_snap = view.clone();
            gtk4::glib::timeout_add_local_once(
                std::time::Duration::from_secs(snapshot_after_secs),
                move || {
                    let snap_path_c = snap_path.clone();
                    view_snap.snapshot(
                        webkit6::SnapshotRegion::Visible,
                        webkit6::SnapshotOptions::NONE,
                        None::<&gtk4::gio::Cancellable>,
                        move |result| match result {
                            Ok(texture) => match texture.save_to_png(&snap_path_c) {
                                Ok(()) => println!(
                                    "joffice: snapshot written to {}",
                                    snap_path_c.display()
                                ),
                                Err(e) => eprintln!("joffice: snapshot write failed: {e}"),
                            },
                            Err(e) => eprintln!("joffice: snapshot failed: {e}"),
                        },
                    );
                    // Give the async snapshot callback time to land,
                    // then exit.
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_secs(5),
                        move || {
                            std::process::exit(0);
                        },
                    );
                },
            );
        }

        *shell.view.borrow_mut() = Some(view.clone());
        // The view now lives inside a GtkBox (banner above it),
        // not as the window's direct child: a box only gives a
        // child the leftover space when the child asks to expand.
        // Without this the WebView keeps a ~0 allocation — the
        // page runs (JS, fonts, onready all fire) but never
        // paints, snapshots fail, and clicks land nowhere.
        // (Cycle-1 drive bisect: marathon-1 binary painted under
        // the identical harness; this was the difference.)
        view.set_hexpand(true);
        view.set_vexpand(true);

        // Console messages to stdout for debugging (WebKitGTK 6.0).
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_enable_write_console_messages_to_stdout(true);
        }

        // Load failures.
        view.connect_load_failed(|_, _event, uri, error| {
            eprintln!("joffice: page load failed ({uri}): {error}");
            false
        });

        // Window layout (marathon 2): a vertical box holding the
        // font-substitution banner (hidden until a substitution
        // happens, §5) above the content — the WebView when a
        // document is open, the native launcher when not.
        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let banner_label = gtk4::Label::new(None);
        banner_label.set_halign(gtk4::Align::Start);
        banner_label.set_margin_start(12);
        banner_label.set_margin_end(12);
        banner_label.set_margin_top(6);
        banner_label.set_margin_bottom(6);
        let banner_revealer = gtk4::Revealer::new();
        banner_revealer.set_child(Some(&banner_label));
        banner_revealer.set_reveal_child(false);
        *shell.banner_label.borrow_mut() = Some(banner_label);
        *shell.banner_revealer.borrow_mut() = Some(banner_revealer.clone());
        vbox.append(&banner_revealer);

        let launcher_holder: Rc<RefCell<Option<gtk4::Box>>> = Rc::new(RefCell::new(None));

        // Open a document in the editor: swap the launcher for
        // the WebView (first open) and load the entry URL. The
        // pending-file machinery (bridge pendingFile) does the
        // rest, exactly as for a --file launch.
        let open_document: Rc<dyn Fn(PathBuf, Option<String>)> = {
            let shell_od = shell.clone();
            let view_od = view.clone();
            let vbox_od = vbox.clone();
            let launcher_od = launcher_holder.clone();
            Rc::new(move |path: PathBuf, as_new: Option<String>| {
                *shell_od.open_as_new.borrow_mut() = as_new;
                *shell_od.pending_file.borrow_mut() = Some(path.clone());
                if let Some(launcher) = launcher_od.borrow_mut().take() {
                    vbox_od.remove(&launcher);
                    vbox_od.append(&view_od);
                }
                // Load the editor through the desktop entry
                // contract (cycle 2): the product enters via
                // apps/api/documents/index.html (deployed from
                // index.html.desktop), whose URL parameters
                // become the DocInfo that gates LocalStartOpen.
                let entry = editor_entry_url(Some(&path));
                view_od.load_uri(&entry);
            })
        };

        if shell.pending_file.borrow().is_some() {
            // --file launch: straight into the editor, as before.
            vbox.append(&view);
            let entry = editor_entry_url(shell.pending_file.borrow().as_ref());
            view.load_uri(&entry);
        } else {
            // No-args launch: the native launcher (Kagi round 2
            // §3/§8 — a start window is a feature, not chrome).
            let launcher = build_launcher(&shell, &window, &open_document);
            *launcher_holder.borrow_mut() = Some(launcher.clone());
            vbox.append(&launcher);
        }

        window.set_child(Some(&vbox));
        // Geometry restore completes before present() (§3).
        if saved_geo.as_ref().map(|g| g.maximized).unwrap_or(false) {
            window.maximize();
        }
        window.present();
    });

    // Run with a FILTERED argv: GtkApplication's own option parser
    // fatally rejects options it does not know ("Unknown option
    // --file"), and ours are parsed above from the real argv. Only
    // the program name goes through; our flags never reach GLib.
    // (Found in cycle 1's first --file test drive: the documented
    // October CLI could never have started the app.)
    let prog = std::env::args().next().unwrap_or_else(|| "joffice-shell".to_string());
    app.run_with_args(&[prog]);
}
