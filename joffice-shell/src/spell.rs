//! Spellcheck service (marathon 2 cycle 7; KAO rounds 1+2,
//! round 2 governing — transcripts in marathon2/).
//!
//! The desktop route, proven from pinned source in Step 0:
//! sdkjs calls `AscDesktopEditor.SpellCheck(JSON.stringify(
//! spellData))` and consumes exactly one reply per task via
//! `window.asc_nativeOnSpellCheck`. One lost reply stalls a
//! document's checking permanently (ContinueSpellCheck starts
//! no new paragraphs while any paragraph waits), so this
//! service answers EVERY task exactly once — with silence
//! (all-correct) when it cannot judge, never with silence of
//! the other kind.
//!
//! Reply construction mirrors stock's native worker
//! (desktop-sdk spellchecker.cpp, read in full in Step 0):
//! the task JSON echoed back with `usrCorrect` (spell) or
//! `usrCorrect:[]` + `usrSuggest:[[...],...]` (suggest)
//! appended; `{"type":"add"}` / `{"type":"clear"}` for those
//! task types; the JSON string `"clear"` for the bare restart.
//! Unknown LCID -> spell `true` (silence), suggest `[]` per
//! word. A word counts correct if the language dictionary OR
//! the user dictionary accepts it.
//!
//! Engine: hand-rolled FFI to the hunspell 1.7 C API,
//! dlopen'd (no link-time dev package needed locally or in
//! the Nix build). All handles live on ONE service thread —
//! hunspell handles are not thread-safe, so they are never
//! shared; suggest gets its own lazily-created handles for
//! the same reason. The (lcid, word) verdict cache holds
//! dictionary verdicts only (language dict + persistent user
//! dict); a protected window's memory-only adds are consulted
//! outside the cache, so nothing about a protected session
//! can leak into another window through it.
//!
//! Wall rule (round 2): spellcheck counts as working only
//! when a self-tested dictionary has judged a word from that
//! window; zero errors is not proof. The liveness machine in
//! main.rs is fed by this service's Judged/NoDict/BrokenDict
//! events.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{c_char, c_int, c_void, CString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// hunspell FFI (dlopen)
// ---------------------------------------------------------------------------

type HunspellCreate = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void;
type HunspellDestroy = unsafe extern "C" fn(*mut c_void);
type HunspellSpell = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type HunspellSuggest =
    unsafe extern "C" fn(*mut c_void, *mut *mut *mut c_char, *const c_char) -> c_int;
type HunspellFreeList = unsafe extern "C" fn(*mut c_void, *mut *mut *mut c_char, c_int);
type HunspellAdd = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;

struct HunspellLib {
    create: HunspellCreate,
    destroy: HunspellDestroy,
    spell: HunspellSpell,
    suggest: HunspellSuggest,
    free_list: HunspellFreeList,
    add: HunspellAdd,
}

fn load_hunspell() -> Result<&'static HunspellLib, String> {
    static LIB: OnceLock<Result<HunspellLib, String>> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let mut names: Vec<CString> = Vec::new();
        if let Ok(p) = std::env::var("JOFFICE_HUNSPELL_LIB") {
            if !p.is_empty() {
                if let Ok(c) = CString::new(p) {
                    names.push(c);
                }
            }
        }
        for n in ["libhunspell-1.7.so.0", "libhunspell-1.7.so", "libhunspell.so.0"] {
            names.push(CString::new(n).unwrap());
        }
        let mut handle: *mut c_void = std::ptr::null_mut();
        let mut tried: Vec<String> = Vec::new();
        for name in &names {
            tried.push(name.to_string_lossy().into_owned());
            let h = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if !h.is_null() {
                handle = h;
                break;
            }
        }
        if handle.is_null() {
            return Err(format!("dlopen failed for {}", tried.join(", ")));
        }
        macro_rules! sym {
            ($name:expr) => {{
                let cname = CString::new($name).unwrap();
                let p = libc::dlsym(handle, cname.as_ptr());
                if p.is_null() {
                    return Err(format!("dlsym {} failed", $name));
                }
                std::mem::transmute(p)
            }};
        }
        Ok(HunspellLib {
            create: sym!("Hunspell_create"),
            destroy: sym!("Hunspell_destroy"),
            spell: sym!("Hunspell_spell"),
            suggest: sym!("Hunspell_suggest"),
            free_list: sym!("Hunspell_free_list"),
            add: sym!("Hunspell_add"),
        })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// One hunspell instance. Owned by the service thread only.
struct Hun {
    ptr: *mut c_void,
    lib: &'static HunspellLib,
}

// The service thread is the only thread that ever touches a
// handle; Send is asserted so the thread can own them.
unsafe impl Send for Hun {}

impl Hun {
    fn create(lib: &'static HunspellLib, aff: &Path, dic: &Path) -> Option<Hun> {
        let caff = CString::new(aff.to_string_lossy().as_bytes()).ok()?;
        let cdic = CString::new(dic.to_string_lossy().as_bytes()).ok()?;
        let ptr = unsafe { (lib.create)(caff.as_ptr(), cdic.as_ptr()) };
        if ptr.is_null() {
            None
        } else {
            Some(Hun { ptr, lib })
        }
    }
    fn spell(&self, word: &str) -> bool {
        let Ok(cw) = CString::new(word) else {
            return false;
        };
        unsafe { (self.lib.spell)(self.ptr, cw.as_ptr()) != 0 }
    }
    fn suggest(&self, word: &str) -> Vec<String> {
        let Ok(cw) = CString::new(word) else {
            return Vec::new();
        };
        let mut list: *mut *mut c_char = std::ptr::null_mut();
        let n = unsafe { (self.lib.suggest)(self.ptr, &mut list, cw.as_ptr()) };
        let mut out = Vec::new();
        if n > 0 && !list.is_null() {
            for i in 0..n as isize {
                let p = unsafe { *list.offset(i) };
                if !p.is_null() {
                    let s = unsafe { std::ffi::CStr::from_ptr(p) }
                        .to_string_lossy()
                        .into_owned();
                    out.push(s);
                }
            }
            unsafe { (self.lib.free_list)(self.ptr, &mut list, n) };
        }
        out
    }
    fn add(&self, word: &str) {
        if let Ok(cw) = CString::new(word) {
            unsafe { (self.lib.add)(self.ptr, cw.as_ptr()) };
        }
    }
}

impl Drop for Hun {
    fn drop(&mut self) {
        unsafe { (self.lib.destroy)(self.ptr) };
    }
}

// ---------------------------------------------------------------------------
// Dictionary registry + self-test (DT1)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DictEntry {
    pub name: String,
    pub lcids: Vec<u32>,
    pub aff: PathBuf,
    pub dic: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DictHealth {
    Ok,
    Broken(String),
}

pub struct Registry {
    pub dir: PathBuf,
    pub entries: Vec<(DictEntry, DictHealth)>,
    pub by_lcid: HashMap<u32, usize>,
    pub installed_lcids: Vec<u32>,
}

/// Load manifest.json from the dictionary dir. The manifest
/// is written by the Nix dictionaries derivation from the
/// pinned Euro-Office/dictionaries tree (English only):
/// {"version":1,"dictionaries":[{"name":"en_US","lcids":[1033]}]}
fn read_manifest(dir: &Path) -> Result<Vec<DictEntry>, String> {
    let path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("SPELL_DICT_MANIFEST unreadable {}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("SPELL_DICT_MANIFEST parse: {e}"))?;
    let arr = v
        .get("dictionaries")
        .and_then(|d| d.as_array())
        .ok_or("SPELL_DICT_MANIFEST: no dictionaries array")?;
    let mut out = Vec::new();
    for item in arr {
        let name = item
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or("SPELL_DICT_MANIFEST: entry without name")?
            .to_string();
        let lcids: Vec<u32> = item
            .get("lcids")
            .and_then(|l| l.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect())
            .unwrap_or_default();
        let sub = dir.join(&name);
        out.push(DictEntry {
            aff: sub.join(format!("{name}.aff")),
            dic: sub.join(format!("{name}.dic")),
            name,
            lcids,
        });
    }
    if out.is_empty() {
        return Err("SPELL_DICT_MANIFEST: empty dictionaries array".into());
    }
    Ok(out)
}

/// DT1 self-test: canary nonsense must be rejected and 20
/// stems sampled from the .dic itself must be accepted. A
/// dictionary that fails is Broken — a different state from
/// missing (round 2 §2), because its words would otherwise be
/// judged by a liar.
fn self_test(lib: &'static HunspellLib, entry: &DictEntry) -> DictHealth {
    let Some(hun) = Hun::create(lib, &entry.aff, &entry.dic) else {
        return DictHealth::Broken("Hunspell_create returned null".into());
    };
    for canary in ["joxqzvwt", "qwzxvkj", "zxqwvbnm"] {
        if hun.spell(canary) {
            return DictHealth::Broken(format!("canary {canary} accepted"));
        }
    }
    let Ok(text) = std::fs::read_to_string(&entry.dic) else {
        return DictHealth::Broken(".dic unreadable as UTF-8".into());
    };
    let stems: Vec<&str> = text
        .lines()
        .skip(1) // the count header
        .map(|l| l.trim().split('/').next().unwrap_or("").trim())
        .filter(|w| w.chars().count() >= 3 && w.chars().all(|c| c.is_alphabetic() || c == '\''))
        .collect();
    if stems.len() < 20 {
        return DictHealth::Broken(format!("only {} usable stems in .dic", stems.len()));
    }
    // 20 evenly spaced samples.
    let step = stems.len() as f64 / 20.0;
    for i in 0..20 {
        let w = stems[(i as f64 * step) as usize];
        if !hun.spell(w) {
            return DictHealth::Broken(format!("sampled stem {w} rejected"));
        }
    }
    DictHealth::Ok
}

// ---------------------------------------------------------------------------
// Word validation + personal dictionary (round 2 §5)
// ---------------------------------------------------------------------------

/// Validate and normalize a word for the personal dictionary.
/// Returns the stored form. Rejects: control/format/bidi
/// characters, any whitespace, `"` and `\` (stock's add path
/// concatenates unescaped JSON), >64 chars / >100 bytes.
/// The curly apostrophe U+2019 normalizes to ASCII `'`.
pub fn validate_word(raw: &str) -> Result<String, String> {
    let normalized: String = raw
        .chars()
        .map(|c| if c == '\u{2019}' { '\'' } else { c })
        .collect();
    let w = normalized.as_str();
    if w.is_empty() {
        return Err("empty".into());
    }
    if w.chars().count() > 64 || w.len() > 100 {
        return Err("too long".into());
    }
    if w.trim() != w {
        return Err("leading/trailing whitespace".into());
    }
    for c in w.chars() {
        if c.is_whitespace() {
            return Err("whitespace".into());
        }
        if c.is_control() {
            return Err("control character".into());
        }
        if matches!(c,
            '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}')
        {
            return Err("format/bidi control".into());
        }
        if c == '"' || c == '\\' {
            return Err("quote/backslash".into());
        }
    }
    Ok(normalized)
}

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("JOFFICE_PERSONAL_DIR") {
        if !d.is_empty() {
            eprintln!("joffice: SPELL_PERSONAL_DIR source=env ({d})");
            return PathBuf::from(d);
        }
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("joffice")
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("joffice/spell-user")
}

/// The personal dictionary store: personal.txt (one word per
/// line) under the write discipline of round 2 §5 — flock,
/// re-read, merge, temp file (O_CREAT|O_EXCL, 0600), fsync,
/// rename, fsync the directory; O_NOFOLLOW on every open of
/// the word file itself; corrupt lines go to
/// personal.rejected and are counted, never silently dropped.
pub struct PersonalStore {
    dir: PathBuf,
    words: BTreeSet<String>,
    readonly: bool,
    add_times: VecDeque<Instant>,
}

const PERSONAL_MAX_WORDS: usize = 100_000;
const PERSONAL_MAX_BYTES: u64 = 4 * 1024 * 1024;
const PERSONAL_RATE_LIMIT: usize = 20; // adds per minute

impl PersonalStore {
    pub fn load() -> PersonalStore {
        let mut store = PersonalStore {
            dir: data_dir(),
            words: BTreeSet::new(),
            readonly: false,
            add_times: VecDeque::new(),
        };
        store.reload_from_disk();
        store
    }

    fn file_path(&self) -> PathBuf {
        self.dir.join("personal.txt")
    }

    /// Read personal.txt with O_NOFOLLOW. A symlink at the
    /// path is a refusal (PD4), not a read.
    fn read_words_file(&self) -> Result<Vec<String>, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.file_path();
        let mut f = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("personal.txt open refused: {e}")),
        };
        let meta = f.metadata().map_err(|e| format!("personal.txt stat: {e}"))?;
        if meta.len() > PERSONAL_MAX_BYTES {
            return Err("personal.txt over 4 MiB size cap".into());
        }
        let mut text = String::new();
        use std::io::Read;
        f.read_to_string(&mut text)
            .map_err(|e| format!("personal.txt read: {e}"))?;
        Ok(text.lines().map(|l| l.to_string()).collect())
    }

    fn reload_from_disk(&mut self) {
        match self.read_words_file() {
            Ok(lines) => {
                let mut rejected: Vec<String> = Vec::new();
                let mut words = BTreeSet::new();
                for line in &lines {
                    if line.is_empty() {
                        continue;
                    }
                    match validate_word(line) {
                        Ok(w) => {
                            words.insert(w);
                        }
                        Err(_) => rejected.push(line.clone()),
                    }
                }
                if words.len() > PERSONAL_MAX_WORDS {
                    self.readonly = true;
                    eprintln!(
                        "joffice: SPELL_PERSONAL_READONLY words={} over cap {PERSONAL_MAX_WORDS}",
                        words.len()
                    );
                }
                if !rejected.is_empty() {
                    eprintln!(
                        "joffice: SPELL_PERSONAL_REJECTED n={} (parked in personal.rejected)",
                        rejected.len()
                    );
                    // Park them immediately so a crash before
                    // the next rewrite does not lose the evidence.
                    let _ = std::fs::create_dir_all(&self.dir);
                    let rej_path = self.dir.join("personal.rejected");
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&rej_path)
                    {
                        for r in &rejected {
                            let _ = writeln!(f, "{r}");
                        }
                    }
                }
                self.words = words;
            }
            Err(e) => {
                eprintln!("joffice: SPELL_PERSONAL_LOAD {e}");
                self.readonly = true;
            }
        }
    }

    pub fn words(&self) -> &BTreeSet<String> {
        &self.words
    }

    /// The locked rewrite: flock(LOCK_EX) on personal.lock,
    /// re-read the file, merge the caller's change against
    /// what is actually on disk NOW (never write from an
    /// in-memory copy alone), atomic rename, dir fsync.
    fn rewrite(&mut self, change: &dyn Fn(&mut BTreeSet<String>)) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("personal dir: {e}"))?;
        {
            let meta =
                std::fs::metadata(&self.dir).map_err(|e| format!("personal dir stat: {e}"))?;
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != unsafe { libc::geteuid() } {
                return Err("personal dir not owned by uid".into());
            }
            if meta.mode() & 0o022 != 0 {
                return Err("personal dir group/world-writable".into());
            }
        }
        let lock_path = self.dir.join("personal.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("personal.lock open: {e}"))?;
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err("personal.lock flock failed".into());
        }
        let result = (|| {
            let lines = self.read_words_file()?;
            let mut merged = BTreeSet::new();
            for line in &lines {
                if line.is_empty() {
                    continue;
                }
                if let Ok(w) = validate_word(line) {
                    merged.insert(w);
                }
            }
            change(&mut merged);
            if merged.len() > PERSONAL_MAX_WORDS {
                return Err("personal dictionary over word cap".into());
            }
            let tmp_path = self.dir.join(format!(
                "personal.txt.tmp-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let mut tmp = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp_path)
                .map_err(|e| format!("personal temp open: {e}"))?;
            for w in &merged {
                writeln!(tmp, "{w}").map_err(|e| format!("personal temp write: {e}"))?;
            }
            tmp.sync_all().map_err(|e| format!("personal temp fsync: {e}"))?;
            drop(tmp);
            std::fs::rename(&tmp_path, self.file_path())
                .map_err(|e| format!("personal rename: {e}"))?;
            if let Ok(d) = std::fs::File::open(&self.dir) {
                let _ = d.sync_all();
            }
            self.words = merged;
            Ok(())
        })();
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
        result
    }

    /// Add one word. Ok(true) = the word is in the dictionary
    /// afterwards (including "already there").
    pub fn add(&mut self, raw: &str) -> Result<bool, String> {
        let word = validate_word(raw)?;
        if self.readonly {
            return Err("personal dictionary is read-only".into());
        }
        let now = Instant::now();
        while let Some(t) = self.add_times.front() {
            if now.duration_since(*t) > Duration::from_secs(60) {
                self.add_times.pop_front();
            } else {
                break;
            }
        }
        if self.add_times.len() >= PERSONAL_RATE_LIMIT {
            eprintln!("joffice: SPELL_PERSONAL_RATELIMIT refused add (>{PERSONAL_RATE_LIMIT}/min)");
            return Err("rate limit".into());
        }
        if self.words.contains(&word) {
            return Ok(true);
        }
        let w2 = word.clone();
        self.rewrite(&|set| {
            set.insert(w2.clone());
        })?;
        self.add_times.push_back(now);
        Ok(true)
    }

    /// {"type":"clear"}: wipe the dictionary (stock semantics:
    /// CreateUserDictionary_All(true) re-seeds empty).
    pub fn clear(&mut self) -> Result<(), String> {
        if self.readonly {
            // The cap state needs a human; stock would wipe.
            // The deviation is ledgered at load
            // (SPELL_PERSONAL_READONLY).
            return Err("personal dictionary is read-only".into());
        }
        self.rewrite(&|set| set.clear())
    }

    /// Regenerate the hunspell user dictionary files
    /// (user.dic + user.aff "SET UTF-8") in the private cache
    /// dir, mirroring stock's <userdicts>/all/all.dic shape.
    pub fn write_user_dict_files(&self) -> Option<(PathBuf, PathBuf)> {
        let dir = cache_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let aff = dir.join("user.aff");
        let dic = dir.join("user.dic");
        if std::fs::write(&aff, "SET UTF-8\n").is_err() {
            return None;
        }
        let mut text = format!("{}\n", self.words.len());
        for w in &self.words {
            text.push_str(w);
            text.push('\n');
        }
        if std::fs::write(&dic, text).is_err() {
            return None;
        }
        Some((aff, dic))
    }
}

// ---------------------------------------------------------------------------
// The service thread
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SpellEvent {
    /// At least one word in a task was judged by a
    /// self-tested (Ok) dictionary.
    Judged { lcid: u32 },
    NoDict { lcid: u32 },
    BrokenDict { lcid: u32 },
}

pub struct SpellReply {
    pub seq: u64,
    /// JSON text to hand to asc_nativeOnSpellCheck. None for
    /// a pure event reply.
    pub payload: Option<String>,
    pub event: Option<SpellEvent>,
}

pub struct SpellJob {
    pub token: u64,
    pub seq: u64,
    pub protected: bool,
    /// The raw task: a JSON string, or the bare string
    /// "clear" for the restart, or CONTROL_UNREGISTER.
    pub task: String,
    pub reply: Sender<SpellReply>,
}

/// Control task (never a real editor task — real tasks are
/// JSON or the bare string "clear"): the window behind this
/// token closed; drop its per-token state.
const CONTROL_UNREGISTER: &str = "jo-control:unregister";

struct TokenState {
    /// Protected windows' memory-only adds (PD7): consulted
    /// for this token only, dropped when the token closes.
    mem_words: HashSet<String>,
    /// Seq of the last bare "clear" (restart): jobs below it
    /// were dropped by the restart and must not be delivered
    /// (the shim dropped their waiters too).
    last_clear_seq: u64,
    was_protected: bool,
}

struct Service {
    lib: &'static HunspellLib,
    registry: Registry,
    /// Spell handles per LCID (only for Ok dictionaries).
    spell_handles: HashMap<u32, Hun>,
    /// Suggest handles per LCID, lazily created, never
    /// shared with the spell path (round 2 §6).
    suggest_handles: HashMap<u32, Hun>,
    /// The user dictionary handle (personal.txt, via the
    /// generated user.dic). Recreated on clear.
    user_handle: Option<Hun>,
    personal: PersonalStore,
    /// Dictionary verdicts only: (lcid, word) -> correct.
    /// Never holds a protected window's memory adds.
    cache: HashMap<(u32, String), bool>,
    tokens: HashMap<u64, TokenState>,
    task_count: u64,
}

impl Service {
    fn user_verdict(&self, word: &str) -> bool {
        match &self.user_handle {
            Some(h) => h.spell(word),
            None => false,
        }
    }

    fn rebuild_user_handle(&mut self) {
        self.user_handle = None;
        if let Some((aff, dic)) = self.personal.write_user_dict_files() {
            self.user_handle = Hun::create(self.lib, &aff, &dic);
        }
    }

    fn judge(
        &mut self,
        token: u64,
        protected: bool,
        word: &str,
        lcid: u32,
    ) -> (bool, Option<SpellEvent>) {
        let idx = match self.registry.by_lcid.get(&lcid) {
            Some(i) => *i,
            None => return (true, Some(SpellEvent::NoDict { lcid })),
        };
        if self.registry.entries[idx].1 != DictHealth::Ok {
            return (true, Some(SpellEvent::BrokenDict { lcid }));
        }
        // Dictionary verdict (cacheable): language dict OR
        // persistent user dict, exactly stock's rule.
        let key = (lcid, word.to_string());
        let mut verdict = match self.cache.get(&key) {
            Some(v) => *v,
            None => {
                let lang_ok = match self.spell_handles.get(&lcid) {
                    Some(h) => h.spell(word),
                    None => false,
                };
                let v = lang_ok || self.user_verdict(word);
                if self.cache.len() > 50_000 {
                    self.cache.clear();
                }
                self.cache.insert(key, v);
                v
            }
        };
        // A protected window's memory-only adds sit OUTSIDE
        // the cache: they die with the token and must never
        // be visible to another window.
        if !verdict && protected {
            if let Some(ts) = self.tokens.get(&token) {
                if ts.mem_words.contains(word) {
                    verdict = true;
                }
            }
        }
        (verdict, Some(SpellEvent::Judged { lcid }))
    }

    fn suggest(&mut self, word: &str, lcid: u32) -> Vec<String> {
        let idx = match self.registry.by_lcid.get(&lcid) {
            Some(i) => *i,
            None => return Vec::new(),
        };
        if self.registry.entries[idx].1 != DictHealth::Ok {
            return Vec::new();
        }
        if !self.suggest_handles.contains_key(&lcid) {
            let entry = &self.registry.entries[idx].0;
            if let Some(h) = Hun::create(self.lib, &entry.aff, &entry.dic) {
                self.suggest_handles.insert(lcid, h);
            } else {
                return Vec::new();
            }
        }
        match self.suggest_handles.get(&lcid) {
            Some(h) => h.suggest(word),
            None => Vec::new(),
        }
    }

    fn process(&mut self, job: SpellJob) {
        if job.task == CONTROL_UNREGISTER {
            self.tokens.remove(&job.token);
            return;
        }
        let token = job.token;
        let seq = job.seq;
        // Track the token; a protected -> unprotected flip
        // drops the memory set (the window changed documents;
        // its protected adds must not follow it).
        {
            let ts = self.tokens.entry(token).or_insert_with(|| TokenState {
                mem_words: HashSet::new(),
                last_clear_seq: 0,
                was_protected: job.protected,
            });
            if ts.was_protected && !job.protected {
                ts.mem_words.clear();
            }
            ts.was_protected = job.protected;
        }
        // Bare restart: drop queued tasks (recognised by seq
        // when they surface), answer with the JSON string
        // "clear".
        if job.task == "clear" {
            if let Some(ts) = self.tokens.get_mut(&token) {
                ts.last_clear_seq = seq;
            }
            let _ = job.reply.send(SpellReply {
                seq,
                payload: Some("\"clear\"".to_string()),
                event: None,
            });
            return;
        }
        // Stale job from before a restart: the shim dropped
        // its waiter when it forwarded the restart; delivering
        // now would hand sdkjs a reply for a paragraph it no
        // longer tracks. Count it, do not deliver.
        if let Some(ts) = self.tokens.get(&token) {
            if seq < ts.last_clear_seq {
                eprintln!("joffice: SPELL_TASK_CLEARED seq={seq} (dropped by restart)");
                return;
            }
        }
        let parsed: serde_json::Value = match serde_json::from_str(&job.task) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("joffice: SPELL_TASK_MALFORMED seq={seq}: {e}");
                let _ = job.reply.send(SpellReply {
                    seq,
                    payload: Some(
                        "{\"type\":\"spell\",\"usrCorrect\":[],\"usrSuggest\":[]}".to_string(),
                    ),
                    event: None,
                });
                return;
            }
        };
        let ty = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
        self.task_count += 1;
        if self.task_count <= 5 || self.task_count % 100 == 0 || ty != "spell" {
            eprintln!("joffice: SPELL_TASK n={} type={} seq={seq}", self.task_count, ty);
        }
        match ty {
            "spell" => {
                let words = task_words(&parsed);
                let langs = parse_langs(&parsed, words.len());
                let mut correct: Vec<bool> = Vec::with_capacity(words.len());
                let mut judged_lcid: Option<u32> = None;
                let mut first_bad: Option<SpellEvent> = None;
                for (w, l) in words.iter().zip(langs.iter()) {
                    let (v, ev) = self.judge(token, job.protected, w, *l);
                    correct.push(v);
                    match ev {
                        Some(SpellEvent::Judged { lcid }) => {
                            if judged_lcid.is_none() {
                                judged_lcid = Some(lcid);
                            }
                        }
                        Some(e) => {
                            if first_bad.is_none() {
                                first_bad = Some(e);
                            }
                        }
                        None => {}
                    }
                }
                let event = match judged_lcid {
                    // The liveness proof outranks a sibling
                    // word's NoDict in the same task; NoDict
                    // LCIDs still surface via their own tasks
                    // (every word of a NoDict paragraph makes
                    // one).
                    Some(lcid) => Some(SpellEvent::Judged { lcid }),
                    None => first_bad,
                };
                let mut reply_obj = parsed.clone();
                if let Some(obj) = reply_obj.as_object_mut() {
                    obj.insert(
                        "usrCorrect".into(),
                        serde_json::Value::Array(
                            correct.iter().map(|b| serde_json::Value::Bool(*b)).collect(),
                        ),
                    );
                    obj.insert("usrSuggest".into(), serde_json::Value::Array(Vec::new()));
                }
                let _ = job.reply.send(SpellReply {
                    seq,
                    payload: Some(reply_obj.to_string()),
                    event,
                });
            }
            "suggest" => {
                let words = task_words(&parsed);
                let langs = parse_langs(&parsed, words.len());
                let mut sugg: Vec<serde_json::Value> = Vec::with_capacity(words.len());
                for (w, l) in words.iter().zip(langs.iter()) {
                    let list = self.suggest(w, *l);
                    sugg.push(serde_json::Value::Array(
                        list.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
                    ));
                }
                let mut reply_obj = parsed.clone();
                if let Some(obj) = reply_obj.as_object_mut() {
                    obj.insert("usrCorrect".into(), serde_json::Value::Array(Vec::new()));
                    obj.insert("usrSuggest".into(), serde_json::Value::Array(sugg));
                }
                let _ = job.reply.send(SpellReply {
                    seq,
                    payload: Some(reply_obj.to_string()),
                    event: None,
                });
            }
            "add" => {
                let words = task_words(&parsed);
                for w in &words {
                    if job.protected {
                        // Memory-only (PD7): validated, held
                        // per token, dropped at close.
                        match validate_word(w) {
                            Ok(v) => {
                                if let Some(ts) = self.tokens.get_mut(&token) {
                                    ts.mem_words.insert(v);
                                }
                                eprintln!(
                                    "joffice: SPELL_ADD_PROTECTED word held in memory only"
                                );
                            }
                            Err(e) => {
                                eprintln!("joffice: SPELL_ADD_REJECTED ({e})");
                            }
                        }
                    } else {
                        match self.personal.add(w) {
                            Ok(_) => {
                                if let Some(h) = &self.user_handle {
                                    h.add(w);
                                }
                                self.cache.clear();
                            }
                            Err(e) => {
                                eprintln!("joffice: SPELL_ADD_FAILED {e}");
                            }
                        }
                    }
                }
                let _ = job.reply.send(SpellReply {
                    seq,
                    payload: Some("{\"type\":\"add\"}".to_string()),
                    event: None,
                });
            }
            "clear" => {
                // JSON clear: wipe + re-seed the user
                // dictionary (stock semantics), and drop this
                // token's memory adds with it.
                if let Some(ts) = self.tokens.get_mut(&token) {
                    ts.mem_words.clear();
                }
                match self.personal.clear() {
                    Ok(()) => {
                        self.rebuild_user_handle();
                        self.cache.clear();
                    }
                    Err(e) => eprintln!("joffice: SPELL_CLEAR_FAILED {e}"),
                }
                let _ = job.reply.send(SpellReply {
                    seq,
                    payload: Some("{\"type\":\"clear\"}".to_string()),
                    event: None,
                });
            }
            _ => {
                eprintln!("joffice: SPELL_TASK_UNKNOWN type={ty} seq={seq} (echoed)");
                let _ = job.reply.send(SpellReply {
                    seq,
                    payload: Some(job.task.clone()),
                    event: None,
                });
            }
        }
    }
}

fn task_words(task: &serde_json::Value) -> Vec<String> {
    task.get("usrWords")
        .and_then(|w| w.as_array())
        .map(|a| a.iter().map(|w| w.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default()
}

/// usrLang is an ARRAY parallel to usrWords (Step 0). A
/// scalar (older clients) applies to every word; a missing
/// or short array pads with 0 (unknown LCID -> silence).
fn parse_langs(task: &serde_json::Value, n_words: usize) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::with_capacity(n_words);
    match task.get("usrLang") {
        Some(serde_json::Value::Array(a)) => {
            for i in 0..n_words {
                out.push(a.get(i).and_then(|v| v.as_u64()).map(|n| n as u32).unwrap_or(0));
            }
        }
        Some(serde_json::Value::Number(num)) => {
            let l = num.as_u64().unwrap_or(0) as u32;
            out.resize(n_words, l);
        }
        _ => out.resize(n_words, 0),
    }
    out
}

// ---------------------------------------------------------------------------
// Global service handle
// ---------------------------------------------------------------------------

pub struct ServiceHandle {
    tx: Sender<SpellJob>,
    pub installed_lcids: Vec<u32>,
    /// True when the service refused to start (no dictionary
    /// dir / no manifest / no engine): tasks are still
    /// answered (silence) through the fallback in submit().
    pub refused: bool,
    pub dict_dir: Option<PathBuf>,
}

static SERVICE: OnceLock<ServiceHandle> = OnceLock::new();
static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

pub fn installed_lcids() -> Vec<u32> {
    SERVICE.get().map(|s| s.installed_lcids.clone()).unwrap_or_default()
}

pub fn service_refused() -> bool {
    SERVICE.get().map(|s| s.refused).unwrap_or(true)
}

/// Initialize the service: resolve the dictionary registry,
/// run DT1 self-tests, spawn the service thread. Called once
/// from main() before the app starts. A missing manifest
/// refuses the service; it never blocks startup.
pub fn init(dict_dir: Option<PathBuf>) {
    let (tx, rx) = std::sync::mpsc::channel::<SpellJob>();
    let mut handle = ServiceHandle {
        tx,
        installed_lcids: Vec::new(),
        refused: true,
        dict_dir: dict_dir.clone(),
    };
    let Some(dir) = dict_dir else {
        eprintln!("joffice: SPELL_SERVICE_REFUSED no dictionary directory resolved");
        let _ = SERVICE.set(handle);
        return;
    };
    let lib = match load_hunspell() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("joffice: SPELL_SERVICE_REFUSED hunspell engine: {e}");
            let _ = SERVICE.set(handle);
            return;
        }
    };
    let entries = match read_manifest(&dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("joffice: SPELL_SERVICE_REFUSED {e}");
            let _ = SERVICE.set(handle);
            return;
        }
    };
    // Self-tests run here, before the service thread takes
    // the dictionaries (DT1 evidence is a startup ledger
    // block, one line per dictionary).
    let mut registry = Registry {
        dir: dir.clone(),
        entries: Vec::new(),
        by_lcid: HashMap::new(),
        installed_lcids: Vec::new(),
    };
    for entry in entries {
        let health = self_test(lib, &entry);
        match &health {
            DictHealth::Ok => {
                eprintln!("joffice: SPELL_DICT_OK name={} lcids={:?}", entry.name, entry.lcids);
                for l in &entry.lcids {
                    registry.by_lcid.insert(*l, registry.entries.len());
                    registry.installed_lcids.push(*l);
                }
            }
            DictHealth::Broken(why) => {
                eprintln!(
                    "joffice: SPELL_DICT_BROKEN name={} lcids={:?}: {why}",
                    entry.name, entry.lcids
                );
                for l in &entry.lcids {
                    registry.by_lcid.insert(*l, registry.entries.len());
                }
            }
        }
        registry.entries.push((entry, health));
    }
    registry.installed_lcids.sort_unstable();
    registry.installed_lcids.dedup();
    handle.installed_lcids = registry.installed_lcids.clone();
    handle.refused = false;

    let personal = PersonalStore::load();
    eprintln!(
        "joffice: SPELL_PERSONAL_LOADED words={} readonly={}",
        personal.words().len(),
        personal.readonly
    );
    let mut service = Service {
        lib,
        registry,
        spell_handles: HashMap::new(),
        suggest_handles: HashMap::new(),
        user_handle: None,
        personal,
        cache: HashMap::new(),
        tokens: HashMap::new(),
        task_count: 0,
    };
    // Pre-create the spell handles for Ok dictionaries so a
    // create failure surfaces at startup, not mid-document.
    let ok_lcids: Vec<u32> = service
        .registry
        .entries
        .iter()
        .filter(|(_, h)| *h == DictHealth::Ok)
        .flat_map(|(e, _)| e.lcids.clone())
        .collect();
    for lcid in ok_lcids {
        let idx = service.registry.by_lcid[&lcid];
        let entry = &service.registry.entries[idx].0;
        match Hun::create(lib, &entry.aff, &entry.dic) {
            Some(h) => {
                service.spell_handles.insert(lcid, h);
            }
            None => {
                eprintln!(
                    "joffice: SPELL_DICT_BROKEN name={} lcid={lcid}: handle create failed at service start",
                    entry.name
                );
                service.registry.entries[idx].1 =
                    DictHealth::Broken("handle create failed at service start".into());
                handle.installed_lcids.retain(|l| *l != lcid);
            }
        }
    }
    service.rebuild_user_handle();

    std::thread::Builder::new()
        .name("joffice-spell".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                service.process(job);
            }
        })
        .expect("spell service thread spawn");

    let _ = SERVICE.set(handle);
}

/// Submit one task from a window. When the service refused
/// to start (or died), the task is still answered exactly
/// once — with silence — through the same reply channel:
/// a broken spellcheck must never stall a document.
pub fn submit(token: u64, seq: u64, protected: bool, task: &str, reply: Sender<SpellReply>) {
    let Some(handle) = SERVICE.get() else {
        let _ = reply.send(fallback_reply(task, seq));
        return;
    };
    if handle.refused {
        let _ = reply.send(fallback_reply(task, seq));
        return;
    }
    let job = SpellJob {
        token,
        seq,
        protected,
        task: task.to_string(),
        reply: reply.clone(),
    };
    if handle.tx.send(job).is_err() {
        eprintln!("joffice: SPELL_SERVICE_DEAD seq={seq} (fallback silence)");
        let _ = reply.send(fallback_reply(task, seq));
    }
}

/// A window closed: drop its per-token state (protected
/// memory adds die with it — PD7). Ordered through the same
/// queue as the token's own tasks.
pub fn unregister(token: u64) {
    if let Some(handle) = SERVICE.get() {
        if handle.refused {
            return;
        }
        let (tx, _rx) = std::sync::mpsc::channel::<SpellReply>();
        let job = SpellJob {
            token,
            seq: u64::MAX,
            protected: false,
            task: CONTROL_UNREGISTER.to_string(),
            reply: tx,
        };
        let _ = handle.tx.send(job);
    }
}

/// The all-correct fallback reply for a task (service
/// refused/dead): echo + silence, stock's unknown-LCID shape.
fn fallback_reply(task: &str, seq: u64) -> SpellReply {
    let mut mk = |payload: String| SpellReply {
        seq,
        payload: Some(payload),
        event: None,
    };
    if task == "clear" {
        return mk("\"clear\"".to_string());
    }
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(task) {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let n_words = v
            .get("usrWords")
            .and_then(|w| w.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        match ty.as_str() {
            "spell" => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "usrCorrect".into(),
                        serde_json::Value::Array(vec![serde_json::Value::Bool(true); n_words]),
                    );
                    obj.insert("usrSuggest".into(), serde_json::Value::Array(Vec::new()));
                }
                return mk(v.to_string());
            }
            "suggest" => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("usrCorrect".into(), serde_json::Value::Array(Vec::new()));
                    obj.insert(
                        "usrSuggest".into(),
                        serde_json::Value::Array(
                            (0..n_words).map(|_| serde_json::Value::Array(Vec::new())).collect(),
                        ),
                    );
                }
                return mk(v.to_string());
            }
            "add" => return mk("{\"type\":\"add\"}".into()),
            "clear" => return mk("{\"type\":\"clear\"}".into()),
            _ => {}
        }
    }
    mk("{\"type\":\"spell\",\"usrCorrect\":[],\"usrSuggest\":[]}".into())
}

// ---------------------------------------------------------------------------
// Hidden CLI self-test (--spell-selftest): DT1 + a protocol
// battery without GTK. The personal dictionary is redirected
// by JOFFICE_PERSONAL_DIR in the harness, never the user's
// real one. Exit 0 = every dictionary Ok + battery green.
// ---------------------------------------------------------------------------

pub fn run_selftest_cli(dict_dir: &Path) -> i32 {
    init(Some(dict_dir.to_path_buf()));
    let (tx, rx) = std::sync::mpsc::channel::<SpellReply>();
    let token = next_token();
    let mut failures = 0;
    let mut seq = 0u64;
    let mut run = |task: &str, failures: &mut i32| -> Option<String> {
        seq += 1;
        submit(token, seq, false, task, tx.clone());
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(r) => r.payload,
            Err(e) => {
                eprintln!("SELFTEST task {task} no reply: {e}");
                *failures += 1;
                None
            }
        }
    };
    if let Some(p) = run(
        "{\"type\":\"spell\",\"ParagraphId\":\"selftest\",\"RecalcId\":1,\"usrWords\":[\"hello\",\"recieve\",\"world\"],\"usrLang\":[1033,1033,1033]}",
        &mut failures,
    ) {
        let ok = p.contains("\"usrCorrect\":[true,false,true]");
        eprintln!("SELFTEST spell en_US: {} ({p})", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    }
    if let Some(p) = run(
        "{\"type\":\"spell\",\"ParagraphId\":\"selftest\",\"RecalcId\":2,\"usrWords\":[\"recieve\"],\"usrLang\":[1053]}",
        &mut failures,
    ) {
        let ok = p.contains("\"usrCorrect\":[true]");
        eprintln!(
            "SELFTEST spell unknown-lcid silence: {}",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            failures += 1;
        }
    }
    if let Some(p) = run(
        "{\"type\":\"suggest\",\"ParagraphId\":\"selftest\",\"RecalcId\":3,\"ElementId\":0,\"usrWords\":[\"recieve\"],\"usrLang\":[1033]}",
        &mut failures,
    ) {
        let ok = p.contains("receive");
        eprintln!(
            "SELFTEST suggest recieve->receive: {}",
            if ok { "PASS" } else { "FAIL" }
        );
        if !ok {
            failures += 1;
        }
    }
    let _ = run("{\"type\":\"add\",\"usrWords\":[\"jofficezqx\"]}", &mut failures);
    if let Some(p) = run(
        "{\"type\":\"spell\",\"ParagraphId\":\"selftest\",\"RecalcId\":4,\"usrWords\":[\"jofficezqx\"],\"usrLang\":[1033]}",
        &mut failures,
    ) {
        let ok = p.contains("\"usrCorrect\":[true]");
        eprintln!("SELFTEST add then spell: {}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    }
    seq += 1;
    submit(token, seq, false, "clear", tx.clone());
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) if r.payload.as_deref() == Some("\"clear\"") => {
            eprintln!("SELFTEST bare clear: PASS");
        }
        Ok(r) => {
            eprintln!("SELFTEST bare clear: FAIL (payload {:?})", r.payload);
            failures += 1;
        }
        Err(e) => {
            eprintln!("SELFTEST bare clear: FAIL (no reply: {e})");
            failures += 1;
        }
    }
    if service_refused() {
        eprintln!("SELFTEST service refused: FAIL");
        failures += 1;
    }
    eprintln!("SELFTEST done failures={failures} installed_lcids={:?}", installed_lcids());
    if failures == 0 {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rules() {
        assert!(validate_word("hello").is_ok());
        assert!(validate_word("don't").is_ok());
        assert_eq!(validate_word("don\u{2019}t").unwrap(), "don't");
        assert!(validate_word("well-known").is_ok());
        assert!(validate_word("").is_err());
        assert!(validate_word("two words").is_err());
        assert!(validate_word(" word").is_err());
        assert!(validate_word("wo\"rd").is_err());
        assert!(validate_word("wo\\rd").is_err());
        assert!(validate_word("wo\u{202e}rd").is_err());
        assert!(validate_word(&"x".repeat(65)).is_err());
        assert!(validate_word("a\u{7}b").is_err());
    }

    #[test]
    fn langs_parsing() {
        let v: serde_json::Value =
            serde_json::from_str("{\"usrWords\":[\"a\",\"b\"],\"usrLang\":[1033,2057]}").unwrap();
        assert_eq!(parse_langs(&v, 2), vec![1033, 2057]);
        let v: serde_json::Value =
            serde_json::from_str("{\"usrWords\":[\"a\",\"b\"],\"usrLang\":1033}").unwrap();
        assert_eq!(parse_langs(&v, 2), vec![1033, 1033]);
        let v: serde_json::Value = serde_json::from_str("{\"usrWords\":[\"a\",\"b\"]}").unwrap();
        assert_eq!(parse_langs(&v, 2), vec![0, 0]);
        let v: serde_json::Value =
            serde_json::from_str("{\"usrWords\":[\"a\",\"b\"],\"usrLang\":[1033]}").unwrap();
        assert_eq!(parse_langs(&v, 2), vec![1033, 0]);
    }

    #[test]
    fn fallback_shapes() {
        let r = fallback_reply(
            "{\"type\":\"spell\",\"usrWords\":[\"a\",\"b\"],\"usrLang\":[1033,1033]}",
            7,
        );
        assert_eq!(r.seq, 7);
        let p = r.payload.unwrap();
        assert!(p.contains("\"usrCorrect\":[true,true]"), "{p}");
        let r = fallback_reply("clear", 9);
        assert_eq!(r.payload.as_deref(), Some("\"clear\""));
    }
}
