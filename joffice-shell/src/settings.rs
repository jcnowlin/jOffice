//! Settings core (marathon 2 cycle 8, KAO rounds 1+2 — round 2
//! governs). One file: `$XDG_CONFIG_HOME/joffice/settings.toml`.
//!
//! The discipline, from round 2 §(d):
//! - The loader NEVER fails. A parse error, a wrong-typed value,
//!   or an out-of-range value yields the safe default for that
//!   key, and the document still opens.
//! - Versioning is by ext4-style feature flags, NOT a schema
//!   integer. Key semantics never change (new meaning = new key
//!   name). Unknown keys are preserved verbatim across writes
//!   (the file is edited as a toml_edit document, never
//!   regenerated from the typed struct).
//! - Writes are atomic (tmp in the same dir + fsync + rename +
//!   dir fsync) under a flock on settings.lock, and every write
//!   keeps the previous file as settings.toml.bak — the config's
//!   own torn-write recovery copy (round 2 never-cut list; this
//!   is NOT document .bak-on-save, which stays unimplemented).
//! - The writer appends a final `# joffice-end` line and records
//!   the file's hash in the state dir. No end line + hash
//!   mismatch => torn: safe defaults, read-only, banner offering
//!   the .bak. End line present + hash mismatch => a hand edit,
//!   accepted.
//! - Never stored (C6): passwords, tokens, secrets, document
//!   content, document paths. The typed struct below is the
//!   complete set of values the writer can emit.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};

pub const END_LINE: &str = "# joffice-end";
pub const DEFAULT_RETENTION_DAYS: i64 = 90;

/// Feature names this build understands. A `[features]` entry
/// marked `incompat` — for ANY name, known or not — puts the
/// store into incompat mode (safe defaults, no writes): the
/// mark can only come from a newer build whose semantics this
/// build cannot know (round 2 §d). An unknown name marked
/// `ro_compat` makes the store read-only but readable.
pub const KNOWN_FEATURES: &[&str] = &["identity", "spell", "autosave", "recovery", "ui", "newdoc"];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Comment/tracked-changes author name. Empty means "not
    /// set": the effective name is generated from the system
    /// account at use time — NEVER the sdkjs default
    /// 'Chuk.Gek'.
    pub username: String,
    /// Stable comment-author identity. Generated once, then
    /// persisted. Never a secret, never an auth token.
    pub uuid: String,
    pub spell_enabled: bool,
    pub spell_ignore_uppercase: bool,
    pub spell_ignore_numbers: bool,
    pub autosave_enabled: bool,
    pub retention_days: i64,
    /// Editor UI language code ("en", "de", ...). One shell
    /// value produces both editorConfig.lang and the URL lang
    /// parameter (round 2 §b sequence 3).
    pub ui_locale: String,
    /// Default language for new documents (BCP-47-ish, e.g.
    /// "en-US"). Only languages with an installed dictionary
    /// are offered in the dialog.
    pub newdoc_language: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            username: String::new(),
            uuid: String::new(),
            spell_enabled: true,
            spell_ignore_uppercase: true,
            spell_ignore_numbers: true,
            autosave_enabled: false,
            retention_days: DEFAULT_RETENTION_DAYS,
            ui_locale: "en".to_string(),
            newdoc_language: "en-US".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoadStatus {
    /// No settings file existed.
    Fresh,
    /// File matches the recorded hash of our last write.
    Clean,
    /// End line present, hash differs: a hand edit, accepted.
    HandEdited,
    /// No end line and the hash does not match (or the file
    /// does not parse): treated as torn. Safe defaults,
    /// read-only, .bak restore offered.
    Torn { reason: String },
    /// A `[features]` entry is marked incompat: safe defaults
    /// for everything, no writes (round 2 §d, test C8).
    Incompat { feature: String },
    /// An unknown feature is marked ro_compat: values load and
    /// are enforced, but this build does not write.
    RoCompat { feature: String },
}

impl LoadStatus {
    pub fn read_only(&self) -> bool {
        matches!(
            self,
            LoadStatus::Torn { .. } | LoadStatus::Incompat { .. } | LoadStatus::RoCompat { .. }
        )
    }
    pub fn banner(&self) -> Option<String> {
        match self {
            LoadStatus::Torn { reason } => Some(format!(
                "Your settings file looks damaged ({reason}). Safe defaults are in use and settings are read-only. You can restore the last good copy (settings.toml.bak) from the Settings dialog."
            )),
            LoadStatus::Incompat { feature } => Some(format!(
                "Your settings file uses a feature this build does not understand ({feature}). Safe defaults are in use and settings are read-only until the file is edited by a newer build."
            )),
            LoadStatus::RoCompat { feature } => Some(format!(
                "Your settings file declares a read-only feature this build does not know ({feature}). Settings are in effect but will not be changed by this build."
            )),
            _ => None,
        }
    }
}

/// FNV-1a 64 — torn-write detection only (answers "are these
/// the bytes we last wrote"), not a security hash.
fn hash_bytes(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Generate a v4 UUID from /dev/urandom (same construction as
/// the cycle-2 identity code).
pub fn gen_uuid() -> String {
    let mut b = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !filled {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b = t.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

fn valid_locale(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 10
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_language(s: &str) -> bool {
    valid_locale(s) && s.len() >= 2
}

pub struct SettingsStore {
    config_dir: PathBuf,
    state_dir: PathBuf,
    /// The parsed document, kept so unknown keys and hand
    /// formatting survive writes verbatim.
    doc: DocumentMut,
    pub settings: Settings,
    pub status: LoadStatus,
    /// Identity imported from the cycle-2 config.toml on first
    /// run (compat): applied only when the settings file does
    /// not carry the key itself.
    pub imported_identity: bool,
}

impl SettingsStore {
    pub fn settings_path(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }
    pub fn path(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }
    pub fn bak_path(&self) -> PathBuf {
        self.config_dir.join("settings.toml.bak")
    }
    fn hash_path(&self) -> PathBuf {
        self.state_dir.join("settings-hash")
    }
    pub fn read_only(&self) -> bool {
        self.status.read_only()
    }

    /// Load (or create the in-memory image of) the settings
    /// store. NEVER fails: any problem degrades to defaults
    /// plus a status that says why. `legacy` carries the
    /// cycle-2 config.toml identity (user_id, user_name) for
    /// the one-time import when settings.toml has none.
    pub fn load(
        config_dir: PathBuf,
        state_dir: PathBuf,
        legacy: (String, String),
    ) -> SettingsStore {
        let path = config_dir.join("settings.toml");
        let mut store = SettingsStore {
            config_dir,
            state_dir,
            doc: DocumentMut::new(),
            settings: Settings::default(),
            status: LoadStatus::Fresh,
            imported_identity: false,
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                // No file: defaults; identity import still
                // applies (first run of the settings build).
                // Persist once so the generated UUID is stable
                // across runs; a read-only config dir makes the
                // save fail, which is tolerated (the in-memory
                // identity still serves this session).
                store.apply_legacy_identity(&legacy);
                store.ensure_uuid();
                let _ = store.save();
                return store;
            }
        };
        let text = match String::from_utf8(bytes.clone()) {
            Ok(t) => t,
            Err(_) => {
                store.status = LoadStatus::Torn {
                    reason: "not valid UTF-8".to_string(),
                };
                store.apply_legacy_identity(&legacy);
                store.ensure_uuid_ephemeral();
                return store;
            }
        };
        let has_end = text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim() == END_LINE)
            .unwrap_or(false);
        let recorded = std::fs::read_to_string(store.hash_path())
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty());
        let hash_matches = recorded.as_deref() == Some(hash_bytes(&bytes).as_str());
        let doc = match text.parse::<DocumentMut>() {
            Ok(d) => d,
            Err(e) => {
                store.status = LoadStatus::Torn {
                    reason: format!("does not parse: {e}"),
                };
                store.apply_legacy_identity(&legacy);
                store.ensure_uuid_ephemeral();
                return store;
            }
        };
        if !has_end && !hash_matches {
            store.status = LoadStatus::Torn {
                reason: "missing end marker and not the last written copy".to_string(),
            };
            store.apply_legacy_identity(&legacy);
            store.ensure_uuid_ephemeral();
            return store;
        }
        store.status = if hash_matches {
            LoadStatus::Clean
        } else {
            LoadStatus::HandEdited
        };
        store.doc = doc;
        // Feature flags (round 2 §d).
        let features = store.read_features();
        for (name, level) in &features {
            if level == "incompat" {
                store.status = LoadStatus::Incompat {
                    feature: name.clone(),
                };
                store.settings = Settings::default();
                store.apply_legacy_identity(&legacy);
                store.ensure_uuid_ephemeral();
                return store;
            }
        }
        for (name, level) in &features {
            if level == "ro_compat" && !KNOWN_FEATURES.contains(&name.as_str()) {
                store.status = LoadStatus::RoCompat {
                    feature: name.clone(),
                };
            }
        }
        store.settings = store.extract();
        store.apply_legacy_identity(&legacy);
        if store.ensure_uuid() && !store.read_only() {
            // Persist a freshly generated UUID so the identity
            // is stable across runs. A failure here is not
            // fatal: the in-memory value still serves.
            let _ = store.save();
        }
        store
    }

    fn ensure_uuid(&mut self) -> bool {
        if self.settings.uuid.is_empty() {
            self.settings.uuid = gen_uuid();
            true
        } else {
            false
        }
    }
    /// In read-only modes the UUID is session-ephemeral: a torn
    /// or foreign file must not be written just to persist it.
    fn ensure_uuid_ephemeral(&mut self) {
        let _ = self.ensure_uuid();
    }

    fn apply_legacy_identity(&mut self, legacy: &(String, String)) {
        let (uid, uname) = legacy;
        if self.settings.uuid.is_empty() && !uid.is_empty() {
            self.settings.uuid = uid.clone();
            self.imported_identity = true;
        }
        if self.settings.username.is_empty() && !uname.is_empty() {
            self.settings.username = uname.clone();
            self.imported_identity = true;
        }
    }

    fn read_features(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(Item::Table(t)) = self.doc.get("features") {
            for (k, v) in t.iter() {
                if let Some(s) = v.as_str() {
                    out.insert(k.to_string(), s.to_string());
                }
            }
        }
        out
    }

    fn extract(&self) -> Settings {
        let d = &self.doc;
        let mut s = Settings::default();
        if let Some(v) = get_str(d, "identity", "username") {
            s.username = v;
        }
        if let Some(v) = get_str(d, "identity", "uuid") {
            if !v.is_empty() {
                s.uuid = v;
            }
        }
        if let Some(v) = get_bool(d, "spell", "enabled") {
            s.spell_enabled = v;
        }
        if let Some(v) = get_bool(d, "spell", "ignore_uppercase") {
            s.spell_ignore_uppercase = v;
        }
        if let Some(v) = get_bool(d, "spell", "ignore_numbers") {
            s.spell_ignore_numbers = v;
        }
        if let Some(v) = get_bool(d, "autosave", "enabled") {
            s.autosave_enabled = v;
        }
        if let Some(v) = get_int(d, "recovery", "retention_days") {
            if (1..=3650).contains(&v) {
                s.retention_days = v;
            }
        }
        if let Some(v) = get_str(d, "ui", "locale") {
            if valid_locale(&v) {
                s.ui_locale = v;
            }
        }
        if let Some(v) = get_str(d, "newdoc", "language") {
            if valid_language(&v) {
                s.newdoc_language = v;
            }
        }
        s
    }

    fn apply_to_doc(&self, doc: &mut DocumentMut) {
        let s = &self.settings;
        set_str(doc, "identity", "username", &s.username);
        set_str(doc, "identity", "uuid", &s.uuid);
        set_bool(doc, "spell", "enabled", s.spell_enabled);
        set_bool(doc, "spell", "ignore_uppercase", s.spell_ignore_uppercase);
        set_bool(doc, "spell", "ignore_numbers", s.spell_ignore_numbers);
        set_bool(doc, "autosave", "enabled", s.autosave_enabled);
        set_int(doc, "recovery", "retention_days", s.retention_days);
        set_str(doc, "ui", "locale", &s.ui_locale);
        set_str(doc, "newdoc", "language", &s.newdoc_language);
    }

    /// Persist the current settings. Atomic, locked, keeps the
    /// previous file as settings.toml.bak, refreshes the hash
    /// record. Refuses in read-only modes (torn / incompat /
    /// ro_compat) — the dialog surfaces the banner instead.
    pub fn save(&mut self) -> Result<(), String> {
        if self.read_only() {
            return Err(format!(
                "settings are read-only ({:?})",
                self.status
            ));
        }
        std::fs::create_dir_all(&self.config_dir)
            .map_err(|e| format!("settings dir: {e}"))?;
        std::fs::create_dir_all(&self.state_dir)
            .map_err(|e| format!("settings state dir: {e}"))?;
        // flock settings.lock for the whole write (C5).
        let lock_path = self.config_dir.join("settings.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("settings lock: {e}"))?;
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err("settings lock: flock failed".to_string());
            }
        }
        let path = self.settings_path();
        // Re-read the on-disk document so hand edits and
        // unknown keys written since load survive; fall back
        // to the in-memory document if it no longer parses.
        let mut base = match std::fs::read_to_string(&path) {
            Ok(text) => text.parse::<DocumentMut>().unwrap_or_else(|_| self.doc.clone()),
            Err(_) => self.doc.clone(),
        };
        // .bak: the previous file, kept on every write.
        if let Ok(prev) = std::fs::read(&path) {
            let bak = self.bak_path();
            if let Ok(mut f) = std::fs::File::create(&bak) {
                let _ = f.write_all(&prev);
                let _ = f.sync_all();
            }
        }
        self.apply_to_doc(&mut base);
        let mut text = base.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        // Manage the end line at the string level: drop an
        // existing one (it may sit above trailing blank lines
        // after an edit) and append exactly one, last.
        let mut lines: Vec<&str> = text.lines().collect();
        if lines.last().map(|l| l.trim()) == Some(END_LINE) {
            lines.pop();
        }
        text = lines.join("\n");
        text.push('\n');
        text.push_str(END_LINE);
        text.push('\n');
        write_atomic(&path, text.as_bytes()).map_err(|e| format!("settings write: {e}"))?;
        self.doc = base;
        self.status = LoadStatus::Clean;
        // Hash record (state dir): the torn-write witness.
        let hash = hash_bytes(text.as_bytes());
        let _ = write_atomic(&self.hash_path(), hash.as_bytes());
        Ok(())
    }

    /// Restore settings.toml.bak over settings.toml (the torn
    /// banner's offer). The restored file carries its end line
    /// but no matching hash record, so the next load accepts it
    /// as a hand edit. Returns the reloaded store.
    pub fn restore_bak(&mut self) -> Result<(), String> {
        let bak = self.bak_path();
        let bytes = std::fs::read(&bak).map_err(|e| format!("restore: no readable .bak: {e}"))?;
        // The .bak must itself parse, or restoring is a second
        // corruption.
        let text = String::from_utf8(bytes).map_err(|_| "restore: .bak is not UTF-8".to_string())?;
        text.parse::<DocumentMut>()
            .map_err(|e| format!("restore: .bak does not parse: {e}"))?;
        std::fs::create_dir_all(&self.config_dir).map_err(|e| format!("restore: {e}"))?;
        write_atomic(&self.settings_path(), text.as_bytes())
            .map_err(|e| format!("restore write: {e}"))?;
        let _ = std::fs::remove_file(self.hash_path());
        Ok(())
    }

    /// One typed mutation + immediate persist (the dialog's
    /// write path: every control writes through, no
    /// apply-on-close — U2).
    pub fn update<F: FnOnce(&mut Settings)>(&mut self, f: F) -> Result<(), String> {
        f(&mut self.settings);
        self.save()
    }
}

fn get_bool(doc: &DocumentMut, table: &str, key: &str) -> Option<bool> {
    doc.get(table)?.get(key)?.as_bool()
}
fn get_str(doc: &DocumentMut, table: &str, key: &str) -> Option<String> {
    doc.get(table)?.get(key)?.as_str().map(|s| s.to_string())
}
fn get_int(doc: &DocumentMut, table: &str, key: &str) -> Option<i64> {
    doc.get(table)?.get(key)?.as_integer()
}

fn table_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    let item = doc
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()));
    match item {
        Item::Table(t) => t,
        _ => {
            *item = Item::Table(Table::new());
            match item {
                Item::Table(t) => t,
                _ => unreachable!(),
            }
        }
    }
}
fn set_str(doc: &mut DocumentMut, table: &str, key: &str, val: &str) {
    table_mut(doc, table)[key] = toml_edit::value(val);
}
fn set_bool(doc: &mut DocumentMut, table: &str, key: &str, val: bool) {
    table_mut(doc, table)[key] = toml_edit::value(val);
}
fn set_int(doc: &mut DocumentMut, table: &str, key: &str, val: i64) {
    table_mut(doc, table)[key] = toml_edit::value(val);
}

/// tmp-in-same-dir + fsync + rename + dir fsync. Shared shape
/// with the document save path's discipline.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn now_unix_pub() -> u64 {
    now_unix()
}

// ---------------------------------------------------------------------------
// Self-test battery (driven by `joffice-shell
// --settings-selftest <scratch-dir>`): the C-series tests
// against real files in a scratch tree, no GTK.
// ---------------------------------------------------------------------------

fn scratch(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let cfg = root.join(name).join("config");
    let state = root.join(name).join("state");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    (cfg, state)
}

const REFERENCE: &str = "\
[identity]
username = \"Ada Lovelace\"
uuid = \"11111111-2222-4333-8444-555555555555\"

[spell]
enabled = false
ignore_uppercase = false
ignore_numbers = true

[autosave]
enabled = true

[recovery]
retention_days = 365

[ui]
locale = \"de\"

[newdoc]
language = \"en-GB\"
# joffice-end
";

fn reference_settings() -> Settings {
    Settings {
        username: "Ada Lovelace".to_string(),
        uuid: "11111111-2222-4333-8444-555555555555".to_string(),
        spell_enabled: false,
        spell_ignore_uppercase: false,
        spell_ignore_numbers: true,
        autosave_enabled: true,
        retention_days: 365,
        ui_locale: "de".to_string(),
        newdoc_language: "en-GB".to_string(),
    }
}

/// Write the reference file + its hash record as if our own
/// writer had produced it.
fn plant_reference(cfg: &Path, state: &Path) {
    std::fs::write(cfg.join("settings.toml"), REFERENCE).unwrap();
    std::fs::write(
        state.join("settings-hash"),
        hash_bytes(REFERENCE.as_bytes()),
    )
    .unwrap();
}

pub fn selftest(root: &Path) -> Vec<(String, bool, String)> {
    let mut out: Vec<(String, bool, String)> = Vec::new();
    let mut run = |name: &str, r: Result<String, String>| match r {
        Ok(detail) => out.push((name.to_string(), true, detail)),
        Err(e) => out.push((name.to_string(), false, e)),
    };

    // C9 (round 1's C1): truncate the reference config at
    // EVERY byte offset: the load never fails, and the values
    // are the originals or the defaults — never a third value.
    run("C9", (|| {
        let (cfg, state) = scratch(root, "c9");
        let bytes = REFERENCE.as_bytes();
        let want = reference_settings();
        let def = Settings::default();
        for cut in 0..=bytes.len() {
            std::fs::write(cfg.join("settings.toml"), &bytes[..cut]).unwrap();
            std::fs::write(state.join("settings-hash"), hash_bytes(bytes)).unwrap();
            let store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
            let s = &store.settings;
            let is_orig = s.username == want.username
                && s.spell_enabled == want.spell_enabled
                && s.retention_days == want.retention_days
                && s.ui_locale == want.ui_locale;
            let is_def = s.username == def.username
                && s.spell_enabled == def.spell_enabled
                && s.retention_days == def.retention_days
                && s.ui_locale == def.ui_locale;
            if !is_orig && !is_def {
                return Err(format!("cut {cut}: third value {s:?}"));
            }
        }
        Ok(format!("{} offsets, original-or-default only", bytes.len() + 1))
    })());

    // C2: an unknown key survives a round trip.
    run("C2", (|| {
        let (cfg, state) = scratch(root, "c2");
        let text = REFERENCE.replace(
            "[ui]",
            "future_thing = \"keep me\"\n\n[ui]",
        );
        std::fs::write(cfg.join("settings.toml"), &text).unwrap();
        std::fs::write(state.join("settings-hash"), hash_bytes(text.as_bytes())).unwrap();
        let mut store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        store.update(|s| s.spell_enabled = true).map_err(|e| e)?;
        let after = std::fs::read_to_string(cfg.join("settings.toml")).unwrap();
        if !after.contains("future_thing = \"keep me\"") {
            return Err("unknown key lost in round trip".to_string());
        }
        Ok("unknown key preserved".to_string())
    })());

    // C3: wrong-typed value -> default; file not rewritten on load.
    run("C3", (|| {
        let (cfg, state) = scratch(root, "c3");
        let text = REFERENCE.replace("retention_days = 365", "retention_days = \"native\"");
        std::fs::write(cfg.join("settings.toml"), &text).unwrap();
        std::fs::write(state.join("settings-hash"), hash_bytes(text.as_bytes())).unwrap();
        let before = std::fs::read(cfg.join("settings.toml")).unwrap();
        let store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if store.settings.retention_days != DEFAULT_RETENTION_DAYS {
            return Err(format!("wrong type not defaulted: {}", store.settings.retention_days));
        }
        let after = std::fs::read(cfg.join("settings.toml")).unwrap();
        if before != after {
            return Err("file rewritten on load".to_string());
        }
        Ok("wrong type defaulted, file untouched".to_string())
    })());

    // C7: NixOS rollback round trip — a newer build's compat
    // keys survive an older build's load + write of its own keys.
    run("C7", (|| {
        let (cfg, state) = scratch(root, "c7");
        let text = REFERENCE.replace(
            "[features]",
            "",
        ) + "\n[features]\nfuture_area = \"compat\"\n";
        // The end line must stay last: rebuild properly.
        let text = text.replace("# joffice-end\n", "") + "# joffice-end\n";
        let text = text.replace(
            "future_area = \"compat\"\n# joffice-end",
            "future_area = \"compat\"\n",
        ) + "# joffice-end\n";
        std::fs::write(cfg.join("settings.toml"), &text).unwrap();
        std::fs::write(state.join("settings-hash"), hash_bytes(text.as_bytes())).unwrap();
        let mut store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if store.read_only() {
            return Err("compat file read-only".to_string());
        }
        store.update(|s| s.retention_days = 30).map_err(|e| e)?;
        let after = std::fs::read_to_string(cfg.join("settings.toml")).unwrap();
        if !after.contains("future_area = \"compat\"") {
            return Err("compat feature entry lost".to_string());
        }
        if !after.contains("retention_days = 30") {
            return Err("own key not written".to_string());
        }
        Ok("compat keys survive, own keys writable".to_string())
    })());

    // C8: an incompat feature set: safe defaults, no writes.
    run("C8", (|| {
        let (cfg, state) = scratch(root, "c8");
        let mut text = REFERENCE.replace("retention_days = 365", "retention_days = 365\n");
        text = text.replace("# joffice-end\n", "");
        text += "\n[features]\nteleport = \"incompat\"\n# joffice-end\n";
        std::fs::write(cfg.join("settings.toml"), &text).unwrap();
        std::fs::write(state.join("settings-hash"), hash_bytes(text.as_bytes())).unwrap();
        let mut store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if !matches!(store.status, LoadStatus::Incompat { .. }) {
            return Err(format!("expected Incompat, got {:?}", store.status));
        }
        if store.settings.retention_days != DEFAULT_RETENTION_DAYS {
            return Err("incompat did not fall back to defaults".to_string());
        }
        if store.update(|s| s.retention_days = 5).is_ok() {
            return Err("incompat store accepted a write".to_string());
        }
        Ok("incompat: defaults + writes refused".to_string())
    })());

    // C5: two writers at once — the flock serialises them and
    // the file is never torn (final file parses, hash matches,
    // end line present).
    run("C5", (|| {
        let (cfg, state) = scratch(root, "c5");
        plant_reference(&cfg, &state);
        let mut handles = Vec::new();
        for n in 0..8 {
            let (c, s) = (cfg.clone(), state.clone());
            handles.push(std::thread::spawn(move || {
                let mut store = SettingsStore::load(c, s, (String::new(), String::new()));
                for i in 0..10 {
                    let v = 100 + ((n * 10 + i) % 50) as i64;
                    let _ = store.update(|st| st.retention_days = v);
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let text = std::fs::read_to_string(cfg.join("settings.toml")).unwrap();
        let store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if store.status != LoadStatus::Clean {
            return Err(format!("final file not Clean: {:?}", store.status));
        }
        if !(1..=3650).contains(&store.settings.retention_days) {
            return Err("final value out of range".to_string());
        }
        Ok("8 writers x 10 writes, file Clean".to_string())
    })());

    // C6: the never-store sweep — a fully populated settings
    // file contains no password/token/secret material, and no
    // document content: the writer can only emit the typed
    // struct's keys.
    run("C6", (|| {
        let (cfg, state) = scratch(root, "c6");
        let mut store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        store.settings = reference_settings();
        store.save().map_err(|e| e)?;
        let text = std::fs::read_to_string(cfg.join("settings.toml")).unwrap();
        let lower = text.to_lowercase();
        for needle in ["password", "passwd", "token", "secret", "credential"] {
            if lower.contains(needle) {
                return Err(format!("settings file contains '{needle}'"));
            }
        }
        // Every key in the file must be one of ours (or the
        // end-line comment): no free-form storage exists.
        let doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
        let allowed: &[&[&str]] = &[
            &["identity", "username"],
            &["identity", "uuid"],
            &["spell", "enabled"],
            &["spell", "ignore_uppercase"],
            &["spell", "ignore_numbers"],
            &["autosave", "enabled"],
            &["recovery", "retention_days"],
            &["ui", "locale"],
            &["newdoc", "language"],
        ];
        for (tname, item) in doc.iter() {
            if tname == "features" {
                continue;
            }
            if let Item::Table(t) = item {
                for (k, _) in t.iter() {
                    let pair: &[&str] = &[tname, k];
                    if !allowed.contains(&pair) {
                        return Err(format!("unexpected key written: {tname}.{k}"));
                    }
                }
            } else {
                return Err(format!("unexpected top-level item: {tname}"));
            }
        }
        Ok("file carries only the typed keys".to_string())
    })());

    // C4 (round 1 numbering): a hand edit (end line kept, hash
    // stale) is accepted with its values.
    run("C4-handedit", (|| {
        let (cfg, state) = scratch(root, "c4");
        plant_reference(&cfg, &state);
        let edited = REFERENCE.replace("retention_days = 365", "retention_days = 42");
        std::fs::write(cfg.join("settings.toml"), &edited).unwrap();
        let store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if store.status != LoadStatus::HandEdited {
            return Err(format!("expected HandEdited, got {:?}", store.status));
        }
        if store.settings.retention_days != 42 {
            return Err("hand-edited value not accepted".to_string());
        }
        Ok("hand edit accepted".to_string())
    })());

    // Torn + restore: a file with no end line and no matching
    // hash loads Torn/read-only; restore_bak brings back the
    // last written copy.
    run("C-torn-restore", (|| {
        let (cfg, state) = scratch(root, "torn");
        let mut store = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        store.settings = reference_settings();
        store.save().map_err(|e| e)?;
        // Second write so the .bak holds the reference values.
        store.update(|s| s.retention_days = 100).map_err(|e| e)?;
        // Tear the live file: drop the tail (and the end line).
        let text = std::fs::read_to_string(cfg.join("settings.toml")).unwrap();
        let cut = &text[..text.len() / 2];
        std::fs::write(cfg.join("settings.toml"), cut).unwrap();
        let mut store2 = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if !matches!(store2.status, LoadStatus::Torn { .. }) {
            return Err(format!("expected Torn, got {:?}", store2.status));
        }
        if !store2.read_only() {
            return Err("torn store not read-only".to_string());
        }
        store2.restore_bak().map_err(|e| e)?;
        let store3 = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        if store3.settings.retention_days != 365 {
            return Err(format!(
                "restored retention {} != 365",
                store3.settings.retention_days
            ));
        }
        Ok("torn detected, .bak restored".to_string())
    })());

    out
}

// ---------------------------------------------------------------------------
// Process-global store + the language producer (cycle 8
// directive 3: ONE function makes both editorConfig.lang
// and the editor URL's lang parameter, from ui.locale).
// ---------------------------------------------------------------------------

static GLOBAL: std::sync::OnceLock<std::sync::Mutex<SettingsStore>> =
    std::sync::OnceLock::new();

/// $XDG_CONFIG_HOME/joffice (glib's user config dir).
pub fn config_dir() -> PathBuf {
    gtk4::glib::user_config_dir().join("joffice")
}

/// $XDG_STATE_HOME/joffice (fallback ~/.local/state/joffice),
/// mirroring the sessions_root base rule in main.rs.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    base.join("joffice")
}

/// Load the store once, at startup, before any window. The
/// load status is ledgered here; the dialog renders it. The
/// legacy identity comes from the cycle-2 config.toml via
/// the crate root's load_config.
pub fn global_init() {
    let legacy = crate::load_config();
    let store = SettingsStore::load(
        config_dir(),
        state_dir(),
        (legacy.user_id, legacy.user_name),
    );
    eprintln!(
        "joffice: SETTINGS_LOADED status={:?} path={}",
        store.status,
        store.path().display()
    );
    let _ = GLOBAL.set(std::sync::Mutex::new(store));
}

pub fn global() -> std::sync::MutexGuard<'static, SettingsStore> {
    GLOBAL
        .get()
        .expect("settings global used before global_init")
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// A snapshot of the current settings values.
pub fn current() -> Settings {
    global().settings.clone()
}

/// The name documents will attribute to this user: the
/// stored username, or — when it is empty — one generated
/// from the system account. NEVER the sdkjs 'Chuk.Gek'
/// fallback (round 2 §b; the dialog warns about exactly
/// this attribution).
pub fn effective_username() -> String {
    let n = current().username;
    if n.trim().is_empty() {
        crate::default_user_name()
    } else {
        n
    }
}

/// The five English dictionaries this build ships, LCID ->
/// BCP-47 tag. The dialog's language list and the locale
/// validation both derive from the spell service's
/// installed set intersected with this table.
pub fn lcid_tag(lcid: u32) -> Option<&'static str> {
    Some(match lcid {
        1033 => "en-US",
        2057 => "en-GB",
        4105 => "en-CA",
        3081 => "en-AU",
        7177 => "en-ZA",
        _ => return None,
    })
}

pub fn tag_lcid(tag: &str) -> Option<u32> {
    [1033u32, 2057, 4105, 3081, 7177]
        .into_iter()
        .find(|&l| lcid_tag(l) == Some(tag))
}

/// The editor language: ui.locale when it names an
/// installed dictionary's tag; else en-US when installed;
/// else the first installed tag; else "en" (the editor's
/// own ultimate fallback). Used for BOTH the entry URL's
/// lang parameter and the shim's editorConfig.lang, so the
/// two can never disagree by construction; the loaded value
/// is still read back and compared (drift detection).
pub fn editor_lang() -> String {
    let locale = current().ui_locale;
    let mut installed: Vec<String> = crate::spell::installed_lcids()
        .iter()
        .filter_map(|&l| lcid_tag(l).map(|t| t.to_string()))
        .collect();
    installed.sort();
    installed.dedup();
    // Exact tag ("en-GB" stored by the dialog's control).
    if installed.iter().any(|t| t == &locale) {
        return locale;
    }
    // Bare language code ("en", the stored default): the
    // family's default tag — en-US for English — else the
    // family's first tag.
    let lang_part = locale.split('-').next().unwrap_or("en");
    let family: Vec<&String> = installed
        .iter()
        .filter(|t| t.split('-').next() == Some(lang_part))
        .collect();
    if let Some(us) = family.iter().find(|t| t.as_str() == "en-US") {
        return us.to_string();
    }
    if let Some(first) = family.first() {
        return first.to_string();
    }
    // The stored locale names no installed dictionary (only
    // reachable by hand edit — the dialog offers installed
    // languages only): fall back to en-US, then any
    // installed tag, then the editor's own "en".
    if installed.iter().any(|t| t == "en-US") {
        return "en-US".to_string();
    }
    installed
        .into_iter()
        .next()
        .unwrap_or_else(|| "en".to_string())
}

/// Tags the new-document language control may offer: only
/// languages with an installed dictionary (round 2 §(b)).
pub fn offered_languages() -> Vec<String> {
    let mut tags: Vec<String> = crate::spell::installed_lcids()
        .iter()
        .filter_map(|&l| lcid_tag(l).map(|t| t.to_string()))
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("joffice-settings-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn selftest_battery_passes() {
        let root = tmpdir("battery");
        let results = selftest(&root);
        for (name, ok, detail) in &results {
            assert!(ok, "{name}: {detail}");
        }
    }

    #[test]
    fn uuid_generated_once_and_persisted() {
        let root = tmpdir("uuid");
        let (cfg, state) = scratch(&root, "u");
        let s1 = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        let s2 = SettingsStore::load(cfg.clone(), state.clone(), (String::new(), String::new()));
        assert!(!s1.settings.uuid.is_empty());
        assert_eq!(s1.settings.uuid, s2.settings.uuid);
    }
}
