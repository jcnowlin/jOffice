//! Export stage ledger + packaging checks (marathon 2 cycle 9,
//! KAO cycle-9 rounds 1–2).
//!
//! The cycle-4 Nix regression recorded a PDF export that
//! reached the shell (453,751 B decoded) and then vanished:
//! no x2t spawn, no PDF, no error, observed >10 s with the
//! editor alive. The surviving log (c4n-export.log) shows the
//! sequence localfilesave → SAVE_STAGE getfile → RUST-SAVE →
//! b64_decode → silence: the saveFile handler decoded the bin
//! and the trail ends inside the routing/dialog span. Three
//! candidate mechanisms (pipeline never calls spawn, spawn
//! dies instantly, reply never delivered) are indistinguishable
//! without a stage ledger. This module is that ledger: one
//! correlation id per export-class save, a stage line at every
//! pipeline transition, and a 10 s per-stage watchdog that
//! NAMES the stage a stalled export is sitting in (round 2's
//! discriminator table). The instrument is itself tested
//! (ST1a–c, `--export-selftest` + the unit tests below): if it
//! cannot tell the three faults apart in a test, it cannot
//! tell them apart in a real stall.
//!
//! Stage vocabulary (round 1 step 5 / round 2 (c)):
//!   RX → DECODED → TMP_WRITTEN → PARAMS → GATE → SPAWN_TRY →
//!   SPAWN_OK(pid) / SPAWN_ERR → EXIT(rc) → terminal
//!   EXPORT_DONE / EXPORT_FAILED(reason).
//! GATE covers the routing decisions; a GATE whose detail is
//! `dialog-parked` is a USER wait (the native Save As dialog)
//! and is exempt from the watchdog — a user reading a dialog
//! is not a stall, and the ledger line still records exactly
//! where the export sat.
//!
//! The reply-ack rule (round 2 (c)): a completion ack carries
//! only a length and a hash, never document content. Save and
//! open replies in this shell never carry document bytes
//! (bytes move by upload reference or URL); `reply_ack`
//! produces the (len, FNV-1a) pair ledgered on both sides of
//! the bridge so a garbled deferred reply shows up as a
//! mismatched pair in a drive log instead of a phantom
//! success.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const STAGE_RX: &str = "RX";
pub const STAGE_DECODED: &str = "DECODED";
pub const STAGE_TMP_WRITTEN: &str = "TMP_WRITTEN";
pub const STAGE_PARAMS: &str = "PARAMS";
pub const STAGE_GATE: &str = "GATE";
pub const STAGE_SPAWN_TRY: &str = "SPAWN_TRY";
pub const STAGE_SPAWN_OK: &str = "SPAWN_OK";
pub const STAGE_SPAWN_ERR: &str = "SPAWN_ERR";
pub const STAGE_EXIT: &str = "EXIT";

/// Per-stage watchdog budget (round 1 step 5: 10 s per stage).
pub const WATCHDOG_BUDGET: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Track {
    stage: &'static str,
    detail: String,
    since: Instant,
    user_wait: bool,
    watchdog_fired: bool,
    history: Vec<&'static str>,
}

struct Ledger {
    tracks: HashMap<i64, Track>,
}

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

fn with_ledger<R>(f: impl FnOnce(&mut Ledger) -> R) -> R {
    let mut guard = LEDGER.lock().unwrap_or_else(|p| p.into_inner());
    let ledger = guard.get_or_insert_with(|| Ledger {
        tracks: HashMap::new(),
    });
    f(ledger)
}

thread_local! {
    /// The correlation id of the export whose conversion is
    /// running on THIS thread (x2t worker threads). Set by the
    /// async conversion wrapper; read by the params writer and
    /// the spawn path, which sit too deep to thread an id
    /// through every signature.
    static CORR: RefCell<Option<i64>> = const { RefCell::new(None) };
}

pub fn set_corr(corr: Option<i64>) {
    CORR.with(|c| *c.borrow_mut() = corr);
}

pub fn corr() -> Option<i64> {
    CORR.with(|c| *c.borrow())
}

/// Begin tracking an export-class save. Emits the RX stage.
pub fn begin(corr: i64, detail: &str) {
    with_ledger(|l| {
        l.tracks.insert(
            corr,
            Track {
                stage: STAGE_RX,
                detail: detail.to_string(),
                since: Instant::now(),
                user_wait: false,
                watchdog_fired: false,
                history: vec![STAGE_RX],
            },
        );
    });
    eprintln!("joffice: EXPORT_STAGE corr={corr} stage={STAGE_RX} {detail}");
}

/// Advance a track to a new stage. `user_wait` marks stages
/// where the pipeline is legitimately parked on the user
/// (native dialog): ledgered, watchdog-exempt.
pub fn stage(corr: i64, stage: &'static str, detail: &str, user_wait: bool) {
    with_ledger(|l| {
        if let Some(t) = l.tracks.get_mut(&corr) {
            t.stage = stage;
            t.detail = detail.to_string();
            t.since = Instant::now();
            t.user_wait = user_wait;
            t.watchdog_fired = false;
            t.history.push(stage);
        }
    });
    eprintln!("joffice: EXPORT_STAGE corr={corr} stage={stage} {detail}");
}

/// Advance the track owned by this thread's correlation id,
/// if any (worker-thread stages: PARAMS / SPAWN_* / EXIT).
pub fn stage_here(stage: &'static str, detail: &str) {
    if let Some(c) = corr() {
        self::stage(c, stage, detail, false);
    }
}

/// Terminal state: EXPORT_DONE / EXPORT_FAILED(reason). The
/// track is removed; a save that never reaches a terminal
/// leaves its track for the watchdog to name.
pub fn terminal(corr: i64, ok: bool, detail: &str) {
    let existed = with_ledger(|l| l.tracks.remove(&corr).is_some());
    if !existed {
        return;
    }
    if ok {
        eprintln!("joffice: EXPORT_DONE corr={corr} {detail}");
    } else {
        eprintln!("joffice: EXPORT_FAILED corr={corr} reason={detail}");
    }
}

/// One watchdog scan (called on the shell's ~1 s pump). Any
/// track parked in a non-user stage past the budget is named
/// once per stage: stage + age + the discriminator reading.
/// Returns the lines emitted (the selftest asserts on them).
pub fn scan() -> Vec<String> {
    let now = Instant::now();
    let mut lines = Vec::new();
    with_ledger(|l| {
        for (corr, t) in l.tracks.iter_mut() {
            if t.user_wait || t.watchdog_fired {
                continue;
            }
            let age = now.duration_since(t.since);
            if age >= WATCHDOG_BUDGET {
                t.watchdog_fired = true;
                let reading = match t.stage {
                    STAGE_SPAWN_OK => {
                        "spawned, no EXIT — worker result or its reply never arrived"
                    }
                    STAGE_SPAWN_TRY => "spawn attempted, neither OK nor ERR recorded",
                    _ => "pipeline never called spawn (no SPAWN_TRY in this track)",
                };
                let line = format!(
                    "joffice: EXPORT_WATCHDOG corr={corr} stage={} age_ms={} detail={} reading=\"{reading}\"",
                    t.stage,
                    age.as_millis(),
                    t.detail
                );
                eprintln!("{line}");
                lines.push(line);
            }
        }
    });
    lines
}

/// Stage history of a track ("" stages joined by >), for the
/// selftest's fault-naming assertions.
pub fn history(corr: i64) -> Vec<&'static str> {
    with_ledger(|l| {
        l.tracks
            .get(&corr)
            .map(|t| t.history.clone())
            .unwrap_or_default()
    })
}

pub fn current_stage(corr: i64) -> Option<&'static str> {
    with_ledger(|l| l.tracks.get(&corr).map(|t| t.stage))
}

/// Test hook: backdate a track's stage clock (ST1b/ST1c age
/// a track past the watchdog budget without sleeping).
#[doc(hidden)]
pub fn backdate(corr: i64, by: Duration) {
    with_ledger(|l| {
        if let Some(t) = l.tracks.get_mut(&corr) {
            t.since = Instant::now() - by;
            t.watchdog_fired = false;
        }
    });
}

pub fn forget(corr: i64) {
    with_ledger(|l| {
        l.tracks.remove(&corr);
    });
}

/// The reply ack: (byte length, FNV-1a 64) of a completion
/// payload's serialization. Length + hash only — the ack
/// never carries content (round 2 (c)).
pub fn reply_ack(serialized: &str) -> (usize, String) {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in serialized.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (serialized.len(), format!("{h:016x}"))
}

/// FM6's launch-time font-map check (round 1 §2, cut order
/// #4 form: version line + count comparison + existence).
/// The generated fonts.log's first line must be the pinned
/// core's version line; every listed path must exist; and
/// every font file in the tree must be listed — the count
/// comparison in set form. (A raw count equality is wrong:
/// the pinned generator also logs a handful of .txt
/// companions it processed, so the listed set is larger
/// than the font-file set by exactly those. What names
/// staleness is a font in the tree the map never saw.)
/// Inside the store, staleness is impossible by
/// construction (the map derivation takes the fonts
/// derivation as an input) — this is the safety net for
/// the shell pointing at a fonts tree the map was not
/// generated from.
pub const EXPECTED_FONTS_VERSION_LINE: &str = "ONLYOFFICE_FONTS_VERSION_15";

pub fn fontmap_check(log_text: &str, fonts_dir: &std::path::Path) -> Result<usize, String> {
    let mut lines = log_text.lines();
    match lines.next() {
        Some(v) if v.trim() == EXPECTED_FONTS_VERSION_LINE => {}
        Some(v) => {
            return Err(format!(
                "font map version line is {v:?}, expected {EXPECTED_FONTS_VERSION_LINE} (pinned core)"
            ));
        }
        None => return Err("fonts.log is empty".to_string()),
    }
    let listed: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
    if listed.is_empty() {
        return Err("fonts.log lists no fonts".to_string());
    }
    let listed_set: std::collections::HashSet<std::path::PathBuf> =
        listed.iter().map(std::path::PathBuf::from).collect();
    for p in &listed {
        if !std::path::Path::new(p).is_file() {
            return Err(format!("fonts.log lists a missing file: {p}"));
        }
    }
    fn walk(
        dir: &std::path::Path,
        listed: &std::collections::HashSet<std::path::PathBuf>,
        unlisted: &mut Option<std::path::PathBuf>,
    ) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            // No symlink following: a symlinked entry is not
            // a font the generator scanned (it dereferences
            // its input tree itself).
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&p, listed, unlisted);
            } else if ft.is_file() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                if (name.ends_with(".ttf") || name.ends_with(".otf") || name.ends_with(".ttc"))
                    && !listed.contains(&p)
                {
                    *unlisted = Some(p);
                    return;
                }
            }
        }
    }
    let mut unlisted = None;
    walk(fonts_dir, &listed_set, &mut unlisted);
    if let Some(p) = unlisted {
        return Err(format!(
            "fonts tree holds a font the map does not list: {}",
            p.display()
        ));
    }
    Ok(listed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ST1c: a pipeline that never calls spawn is named as
    /// such — the track sits at GATE, the watchdog fires, and
    /// the reading says no SPAWN_TRY was recorded.
    #[test]
    fn st1c_never_spawns_is_named() {
        let corr = 991_003;
        begin(corr, "selftest");
        stage(corr, STAGE_GATE, "decision=bound", false);
        backdate(corr, WATCHDOG_BUDGET + Duration::from_secs(1));
        let lines = scan();
        forget(corr);
        assert!(
            lines.iter().any(|l| l.contains("corr=991003")
                && l.contains("stage=GATE")
                && l.contains("never called spawn")),
            "watchdog must name the never-spawn fault: {lines:?}"
        );
        assert!(!history(corr).contains(&STAGE_SPAWN_TRY));
    }

    /// ST1b: a spawn whose reply never arrives is named
    /// differently — the track sits at SPAWN_OK with no EXIT.
    #[test]
    fn st1b_reply_never_arrives_is_named() {
        let corr = 991_002;
        begin(corr, "selftest");
        stage(corr, STAGE_GATE, "decision=bound", false);
        stage(corr, STAGE_SPAWN_TRY, "x2t", false);
        stage(corr, STAGE_SPAWN_OK, "pid=4242", false);
        backdate(corr, WATCHDOG_BUDGET + Duration::from_secs(1));
        let lines = scan();
        forget(corr);
        assert!(
            lines.iter().any(|l| l.contains("corr=991002")
                && l.contains("stage=SPAWN_OK")
                && l.contains("never arrived")),
            "watchdog must name the lost-reply fault: {lines:?}"
        );
    }

    /// A user-wait stage (native dialog parked) never trips
    /// the watchdog, however long the user takes.
    #[test]
    fn dialog_wait_is_watchdog_exempt() {
        let corr = 991_004;
        begin(corr, "selftest");
        stage(corr, STAGE_GATE, "dialog-parked", true);
        backdate(corr, WATCHDOG_BUDGET * 30);
        let lines = scan();
        forget(corr);
        assert!(
            !lines.iter().any(|l| l.contains("corr=991004")),
            "dialog waits are not stalls: {lines:?}"
        );
    }

    #[test]
    fn ack_is_len_and_hash_only() {
        let (len, hash) = reply_ack("{\"ok\":true}");
        assert_eq!(len, 11);
        assert_eq!(hash.len(), 16);
    }

    #[test]
    fn fontmap_check_accepts_and_refuses() {
        let dir = std::env::temp_dir().join(format!("jo-fm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.ttf"), b"x").unwrap();
        std::fs::write(dir.join("sub/b.otf"), b"x").unwrap();
        let good = format!(
            "{EXPECTED_FONTS_VERSION_LINE}\n{}\n{}\n",
            dir.join("a.ttf").display(),
            dir.join("sub/b.otf").display()
        );
        assert_eq!(fontmap_check(&good, &dir), Ok(2));
        // A listed non-font companion (the pinned
        // generator logs .txt files it processed) is
        // existence-checked but does not enter the
        // tree-side font set.
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let with_txt = format!("{good}{}\n", dir.join("notes.txt").display());
        assert_eq!(fontmap_check(&with_txt, &dir), Ok(3));
        std::fs::remove_file(dir.join("notes.txt")).unwrap();
        // Wrong version line.
        let bad_v = good.replace(EXPECTED_FONTS_VERSION_LINE, "ONLYOFFICE_FONTS_VERSION_12");
        assert!(fontmap_check(&bad_v, &dir).is_err());
        // Count mismatch (a font the map does not know).
        std::fs::write(dir.join("c.ttf"), b"x").unwrap();
        assert!(fontmap_check(&good, &dir).is_err());
        // Dangling listed path (the count agrees; the file
        // does not exist).
        std::fs::remove_file(dir.join("c.ttf")).unwrap();
        let dangling = format!(
            "{EXPECTED_FONTS_VERSION_LINE}\n{}\n{}\n{}\n",
            dir.join("a.ttf").display(),
            dir.join("sub/b.otf").display(),
            dir.join("gone.ttf").display()
        );
        assert!(fontmap_check(&dangling, &dir).is_err());
        std::fs::write(dir.join("gone.ttf"), b"x").unwrap();
        assert_eq!(fontmap_check(&dangling, &dir), Ok(3));
        std::fs::remove_file(dir.join("gone.ttf")).unwrap();
        assert!(fontmap_check(&dangling, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
