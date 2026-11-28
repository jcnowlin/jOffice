//! Notice policy (marathon 2 cycle 8, KAO round 2 §(e)).
//!
//! Two classes:
//!
//! SAFETY notices can never be persistently suppressed. The
//! user may dismiss one for the life of the window; it
//! shrinks to a badge and returns on reopen. The quiet API
//! refuses safety kinds outright (test N1).
//!
//! INFORMATIONAL notices (font substitutions per kind,
//! NoDict per LCID) may be quieted; the grants live in the
//! state dir (notice-state.json), keyed by kind (+ LCID),
//! and NEVER carry a document identifier (test N2).

use std::collections::HashSet;
use std::path::Path;

/// The safety list. Membership here is a product decision,
/// recorded in marathon2/CYCLES.md cycle 8.
pub const SAFETY_NOTICES: &[&str] = &[
    "pdf-disclosure",    // cycle 5: annots merge semantics
    "protected-banner",  // cycle 6: recovery off for protected docs
    "autosave-paused",   // conflict / ENOSPC pause
    "autosave-off",      // autosave could not arm (no revert copy, gate)
    "save-dropped",      // cycle 8 dropped-save detector
    "spell-dead",        // spell service Dead
    "spell-wrongroute",  // reply arrived via the wrong route
    "spell-capped",      // 2000-error cap hit
    "settings-drift",    // effective value != configured value
    "sessions-expiring", // retention: sessions awaiting a decision
];

pub fn is_safety(kind: &str) -> bool {
    SAFETY_NOTICES.contains(&kind)
}

#[derive(Default, Clone, Debug)]
pub struct NoticeState {
    /// Quieted informational grants: "kind" or "kind:<lcid>".
    quieted: HashSet<String>,
}

impl NoticeState {
    pub fn load(state_dir: &Path) -> NoticeState {
        let path = state_dir.join("notice-state.json");
        let mut st = NoticeState::default();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(arr) = v.get("quieted").and_then(|x| x.as_array()) {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            // Safety kinds are refused even
                            // from disk (a hand-edited state
                            // file cannot suppress them).
                            if !is_safety(s) {
                                st.quieted.insert(s.to_string());
                            }
                        }
                    }
                }
            }
        }
        st
    }

    pub fn save(&self, state_dir: &Path) {
        let v = serde_json::json!({
            "quieted": self.quieted.iter().collect::<Vec<_>>(),
        });
        let path = state_dir.join("notice-state.json");
        let _ = crate::settings::write_atomic(&path, v.to_string().as_bytes());
    }

    /// Quiet an informational notice kind (optionally per
    /// LCID). Returns false — and stores nothing — for
    /// safety kinds (N1).
    pub fn quiet(&mut self, kind: &str, lcid: Option<u32>) -> bool {
        if is_safety(kind) {
            return false;
        }
        let key = match lcid {
            Some(l) => format!("{kind}:{l}"),
            None => kind.to_string(),
        };
        self.quieted.insert(key);
        true
    }

    pub fn is_quieted(&self, kind: &str, lcid: Option<u32>) -> bool {
        if is_safety(kind) {
            return false;
        }
        if self.quieted.contains(kind) {
            return true;
        }
        match lcid {
            Some(l) => self.quieted.contains(&format!("{kind}:{l}")),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n1_safety_cannot_be_quieted() {
        let mut st = NoticeState::default();
        for kind in SAFETY_NOTICES {
            assert!(!st.quiet(kind, None), "safety kind {kind} was quieted");
            assert!(!st.is_quieted(kind, None));
        }
        // And a hand-forged state file cannot smuggle one in.
        let dir = std::env::temp_dir().join(format!(
            "joffice-notices-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("notice-state.json"),
            "{\"quieted\":[\"save-dropped\",\"font-subst\"]}",
        )
        .unwrap();
        let st2 = NoticeState::load(&dir);
        assert!(!st2.is_quieted("save-dropped", None));
        assert!(st2.is_quieted("font-subst", None));
    }

    #[test]
    fn n2_informational_quieting_is_per_kind_and_lcid() {
        let mut st = NoticeState::default();
        assert!(st.quiet("nodict", Some(1033)));
        assert!(st.is_quieted("nodict", Some(1033)));
        assert!(!st.is_quieted("nodict", Some(2057)));
        assert!(st.quiet("font-subst", None));
        assert!(st.is_quieted("font-subst", Some(1033)));
    }
}
