// AscDesktopEditor host shim for jOffice.
//
// sdkjs does NOT talk to window.joffice.invoke. It talks to
// window.AscDesktopEditor — the desktop-host interface that ONLYOFFICE
// DesktopEditors and euro-office-lite implement. This shim implements
// that interface on top of the Rust bridge (window.joffice.invoke).
//
// Correct lifecycle (Kagi round 2 — sdkjs common/Local/common.js):
//   LocalStartOpen -> bridge convert -> DesktopOfflineAppDocumentEndLoad
//   (which builds OpenFileResult, checks the stream signature, sets
//   file.url, calls editor.openDocument, asc_SetFastCollaborative,
//   DesktopOfflineUpdateLocalName, DesktopAfterOpen, password event).
// Do NOT hand-build OpenFileResult / call editor.openDocument directly.
//
// Flow for opening a local file:
//   1. sdkjs calls AscDesktopEditor.LocalStartOpen()
//   2. Shim asks Rust for the pending file (if any) -> convert via x2t
//   3. Shim calls window.DesktopOfflineAppDocumentEndLoad(url, data, len)
// Flow for saving:
//   1. sdkjs calls AscDesktopEditor.LocalFileSave(...)
//   2. Shim pulls Editor.bin from the editor via asc_nativeGetFile()
//   3. Shim asks Rust to convert Editor.bin back via x2t and write it
//
// This file is injected into the TOP frame only, before any page script,
// on the joffice:// origin. It must not run on any other origin.

(function () {
    'use strict';

    // The Rust bridge. Every call returns a Promise. Guard: if the
    // handler is absent (e.g. shim ran on a page without the bridge),
    // fail loudly instead of throwing on undefined.
    function bridge(cmd, args) {
        var handlers = window.webkit && window.webkit.messageHandlers;
        if (!handlers || !handlers.joffice) {
            return Promise.reject(new Error('joffice bridge handler missing'));
        }
        args = args || {};
        // Cycle 4 hardening: every bridge call carries the
        // per-process secret Rust injected as window.__joSecret.
        // A frame the config script never reached (a foreign
        // subframe) cannot mint valid calls.
        if (window.__joSecret && args.secret === undefined) {
            args.secret = window.__joSecret;
        }
        return handlers.joffice.postMessage({ cmd: cmd, args: args });
    }

    // The editor API object sdkjs hands us via CreateEditorApi.
    var editorApi = null;
    // Guard against LocalStartOpen firing twice (reopen loop).
    var startOpenDone = false;
    // Current document name, for SetDocumentName / DesktopOfflineUpdateLocalName.
    var currentDocName = '';

    // Find the editor API: prefer the object from CreateEditorApi,
    // fall back to scanning iframes for contentWindow.Asc.
    function getEditor() {
        if (editorApi) return { editor: editorApi, ew: window };
        var frames = document.querySelectorAll('iframe');
        for (var i = 0; i < frames.length; i++) {
            try {
                if (frames[i].contentWindow && frames[i].contentWindow.Asc) {
                    var api = frames[i].contentWindow.Asc.editor;
                    if (api) return { editor: api, ew: frames[i].contentWindow };
                }
            } catch (e) { /* cross-origin frame, skip */ }
        }
        return { editor: null, ew: null };
    }

    // -----------------------------------------------------------
    // Save/dirty machinery (marathon 2 cycle 2; Kagi round 2
    // §B/§C/§F is the design authority).
    //
    // joSave is exposed as window.__joSave: the shim runs once
    // per frame, and the copy that matters lives in the EDITOR
    // frame (that's where StartSave/LocalFileSave run). The top
    // frame's copy is a bystander; Rust's close helpers reach
    // through getEditor().ew.__joSave to read the editor frame's
    // flags.
    // -----------------------------------------------------------
    var joSave = {
        seq: 0,               // last assigned save sequence number
        pendingSeq: null,     // seq still owed its one EndSave
        started: false,       // StartSave fired during a requested save
        startSaveSeen: false, // StartSave wrap has fired at least once
        startSaveOpen: false, // a StartSave cycle is open (cycle 4 alarm)
        boundSaved: false,    // LocalFileGetSaved value
        lastPushedModified: null, // last onDocumentModifiedChanged value
        overlayForced: false,     // last push was journal-overlay-forced (W2)
        handshakeSent: false,
        cellEditing: false,
        replyWaiters: {},     // seq -> settle fn (dual-channel replies)
        nextOrigin: null      // 'changes' when LocalFileSaveChanges delegates
    };
    window.__joSave = joSave;

    // -----------------------------------------------------------
    // Spellcheck (marathon 2 cycle 7; KAO round 2 §2/§4).
    //
    // sdkjs (desktop branch, armed by _coSpellCheckInit via
    // the auth fake-call) calls AscDesktopEditor.SpellCheck(
    // JSON.stringify(spellData)) and consumes exactly one
    // reply per task through window.asc_nativeOnSpellCheck.
    // One lost reply stalls the document's checking
    // permanently, so every task is registered as a waiter
    // here and settled exactly once: by Rust's JS-channel
    // delivery (__joDeliverSpellReply), by the bridge-failure
    // fallback, or by the watchdog — the last two with a
    // locally built all-correct echo (silence, never a stall).
    // -----------------------------------------------------------
    var joSpell = {
        seq: 0,
        sent: 0,
        answered: 0,
        waiters: {} // seq -> {task, paraId, at}
    };
    window.__joSpell = joSpell;
    var joSpellProbe = { disabled: null, capped: null, editedSentAt: 0 };

    // The all-correct echo for a task (stock's unknown-LCID
    // shape), built locally when Rust cannot answer.
    function joSpellFallback(taskStr) {
        var t = null;
        try { t = JSON.parse(taskStr); } catch (e) { /* fall through */ }
        if (!t || typeof t !== 'object') {
            return { type: 'spell', usrCorrect: [], usrSuggest: [] };
        }
        if (t.type === 'spell') {
            var n = Array.isArray(t.usrWords) ? t.usrWords.length : 0;
            t.usrCorrect = [];
            for (var i = 0; i < n; i++) t.usrCorrect.push(true);
            t.usrSuggest = [];
            return t;
        }
        if (t.type === 'suggest') {
            var m = Array.isArray(t.usrWords) ? t.usrWords.length : 0;
            t.usrCorrect = [];
            t.usrSuggest = [];
            for (var j = 0; j < m; j++) t.usrSuggest.push([]);
            return t;
        }
        if (t.type === 'add') return { type: 'add' };
        if (t.type === 'clear') return { type: 'clear' };
        return t;
    }

    // Settle one waiter with a payload value (an object, or
    // the string 'clear'). Returns false when the payload's
    // ParagraphId belongs to a different task — the seq
    // spaces of two frames can collide, and the deliver
    // function's forwarding then tries the other frame.
    function joSpellTake(seq, payload) {
        var w = joSpell.waiters[seq];
        if (!w) return false;
        var val = payload;
        var bad = false;
        if (val !== 'clear' && (typeof val !== 'object' || val === null)) bad = true;
        if (!bad && val !== 'clear' && w.paraId != null
            && val.ParagraphId !== undefined
            && String(val.ParagraphId) !== String(w.paraId)) {
            return false;
        }
        delete joSpell.waiters[seq];
        joSpell.answered++;
        if (bad) {
            joLedger('SPELL_REPLY_GARBLED', seq, {});
            val = joSpellFallback(w.task);
        }
        if (typeof window.asc_nativeOnSpellCheck === 'function') {
            try { window.asc_nativeOnSpellCheck(val); }
            catch (e) { console.log('[jOffice] asc_nativeOnSpellCheck threw: ' + e); }
        } else {
            joLedger('SPELL_NO_CALLBACK', seq, {});
        }
        return true;
    }

    // Dual-channel spell-reply delivery: Rust evaluates this
    // in the top frame; the top frame forwards to the editor
    // frame, whose copy settles the seq's waiter (the save /
    // open pattern).
    window.__joDeliverSpellReply = function (seq, payload) {
        if (joSpellTake(seq, payload)) return true;
        var ref = getEditor();
        if (ref.ew && ref.ew !== window && ref.ew.__joDeliverSpellReply) {
            return ref.ew.__joDeliverSpellReply(seq, payload);
        }
        return false;
    };

    // The watchdog: a waiter older than 10 s is settled with
    // the fallback echo and ledgered on BOTH channels (the
    // CR1 grade counts SPELL_WATCHDOG in the shell's stderr).
    setInterval(function () {
        var now = Date.now();
        for (var seq in joSpell.waiters) {
            var w = joSpell.waiters[seq];
            if (w && now - w.at > 10000) {
                delete joSpell.waiters[seq];
                joSpell.answered++;
                joLedger('SPELL_WATCHDOG', Number(seq), { age_ms: now - w.at });
                bridge('spellEvent', { kind: 'watchdog', seq: Number(seq) }).catch(function () {});
                var val = joSpellFallback(w.task);
                if (typeof window.asc_nativeOnSpellCheck === 'function') {
                    try { window.asc_nativeOnSpellCheck(val); } catch (e) { /* frame going away */ }
                }
            }
        }
    }, 2000);

    // Liveness probes (round 2 §2): the word editor's
    // CDocumentSpellChecker lives at
    // editor.WordControl.m_oLogicDocument.Spelling and
    // carries ErrorsExceed + IsOn(). Transitions only —
    // the shell's state machine does the rest. Cell/slide
    // expose no equivalent object on this path; their
    // Disabled/Capped states stay unprobed (recorded in
    // CYCLES.md), their Live/NoDict states still prove out
    // through tasks.
    setInterval(function () {
        var ref = getEditor();
        var ed = ref.editor;
        if (!ed) return;
        var sp = null;
        try {
            sp = ed.WordControl && ed.WordControl.m_oLogicDocument
                && ed.WordControl.m_oLogicDocument.Spelling;
        } catch (e) { /* not a word editor */ }
        if (!sp) return;
        var capped = null, on = null;
        try { capped = !!sp.ErrorsExceed; } catch (e) { /* ignore */ }
        try { on = (typeof sp.IsOn === 'function') ? !!sp.IsOn() : null; } catch (e) { /* ignore */ }
        if (capped !== null && capped !== joSpellProbe.capped) {
            joSpellProbe.capped = capped;
            if (capped) {
                joLedger('SPELL_PROBE_CAPPED', null, {});
                bridge('spellEvent', { kind: 'capped' }).catch(function () {});
            }
        }
        if (on !== null) {
            var dis = !on;
            if (dis !== joSpellProbe.disabled) {
                joSpellProbe.disabled = dis;
                bridge('spellEvent', { kind: dis ? 'disabled' : 'enabled' }).catch(function () {});
            }
        }
    }, 3000);

    // Language-list filter (round 2 §2): stock's
    // DesktopAfterOpen advertises every key of
    // spellcheckGetLanguages() as spellcheck-capable whether
    // or not a dictionary exists. Wrap the editor's sendEvent
    // before that can happen (CreateEditorApi fires first)
    // and advertise only the LCIDs Rust installed.
    function joInstallSpellFilter(api) {
        if (!api || api.__joSpellFilter || typeof api.sendEvent !== 'function') return;
        api.__joSpellFilter = true;
        var installed = {};
        (window.__joSpellLcids || []).forEach(function (n) { installed[Number(n)] = true; });
        var orig = api.sendEvent.bind(api);
        api.sendEvent = function (name) {
            var a = Array.prototype.slice.call(arguments, 1);
            if (name === 'asc_onSpellCheckInit' && Array.isArray(a[0])) {
                var before = a[0].length;
                a[0] = a[0].filter(function (c) { return !!installed[Number(c)]; });
                joLedger('SPELL_LANG_FILTER', null, { before: before, after: a[0].length });
            }
            return orig.apply(null, [name].concat(a));
        };
    }

    // Spellcheck on/off toggles from the editor UI arrive as
    // asc_setSpellCheck on the api object: mirror them to
    // the shell's liveness machine.
    function joWrapSpellToggle(api) {
        if (!api || api.__joSpellToggle || typeof api.asc_setSpellCheck !== 'function') return;
        api.__joSpellToggle = true;
        var orig = api.asc_setSpellCheck.bind(api);
        api.asc_setSpellCheck = function (isOn) {
            joLedger('SPELL_TOGGLE', null, { on: isOn ? 'yes' : 'no' });
            bridge('spellEvent', { kind: isOn ? 'enabled' : 'disabled' }).catch(function () {});
            return orig(isOn);
        };
    }

    // -----------------------------------------------------------
    // Cycle 4: flush mode + journal state. The mode is injected
    // by Rust (window.__joFlushMode, from JOFFICE_FLUSH_MODE):
    //   'journal' — mode J (production): saveChanges is journaled
    //     by the wrap below; LocalFileSaveChanges is log-only.
    //   'bound'   — cycle-3 behavior: the flush delegates to a
    //     full LocalFileSave (the fallback if E0 condemns J).
    //   'm0'/'m1'/'m2'/'m3' — the E0 experiment modes.
    // -----------------------------------------------------------
    function joFlushMode() {
        return window.__joFlushMode || 'journal';
    }
    // The journal machinery (recorder, UNMOD gate, modified
    // overlay, open-changes count) applies to the production
    // 'journal' mode and to E0's 'm1' alias alike: m1 IS the
    // journal design under its experiment label. Before this
    // helper existed the gates tested === 'journal' exactly,
    // so an m1 run journaled nothing and the section-6a
    // unmodified shortcut could eat edits on Save As.
    function joJournalMode() {
        var m = joFlushMode();
        return m === 'journal' || m === 'm1';
    }
    var joJournal = {
        epoch: 0,          // current journal epoch
        seq: 0,            // last assigned journal seq (this epoch)
        base: 0,           // history items folded into the epoch seed
        foldedLen: 0,      // items in seed + journal (the fold)
        openChanges: 0,    // LocalFileGetOpenChangesCount value
        sid: null,         // session id (from the open reply)
        enabled: false,    // journal active for this document
        wrapAsserted: false,
        checkpointBusy: false,
        imageStaged: false, // an image was inserted since the last checkpoint (K6)
        histLen: 0,        // items through History.Index (last send)
        boundHistLen: 0    // histLen as of the last bound save
    };
    window.__joJournal = joJournal;

    // -----------------------------------------------------------
    // Cycle 5: PDF window state + PE0 probes.
    // joPdf.active is set from the open reply (rawPdf) when the
    // shell opened the PDF under the annotate dev flag
    // (pdfMode in the reply). PDF windows journal append-only
    // and never checkpoint (KAO cycle-5 round 2, worst-case
    // session model): the cycle-4 fold/checkpoint machinery
    // reads a bin snapshot (asc_nativeGetFile3) that is
    // meaningless for a PDF, and a fold with a misread
    // deleteIndex would resurrect or drop annotations on
    // recovery. PDF recovery is state-based (compiled
    // snapshots), recovery_degraded = true.
    // -----------------------------------------------------------
    var joPdf = {
        active: false, mode: null, degradedLogged: false,
        protected: false, perms: null
    };
    // Protected cycle: true while this window's document is
    // password-protected (any format — set from the open
    // reply's `protected` flag).
    var joProtectedDoc = false;
    window.__joPdf = joPdf;

    // -----------------------------------------------------------
    // Protected cycle: the PDF password hand-off + the
    // clipboard gate. Rust probed the password itself (x2t)
    // and read the permission bits itself (qpdf); the engine
    // is the decryptor, so the password crosses ONCE through
    // the open reply into joPdfPw, the createFile wrap feeds
    // it to the engine, and the reference is dropped. The
    // password is never logged, never stored, never sent
    // anywhere else.
    // -----------------------------------------------------------
    var joPdfPw = null;

    function joInstallPdfPasswordWrap(retries) {
        if (window.AscViewer && window.AscViewer.__joPwWrapped) return;
        if (!window.AscViewer || !window.AscViewer.createFile
            || !window.AscViewer.setFilePassword) {
            // The viewer namespace loads with the PDF editor;
            // the open reply can land first. Poll briefly —
            // the api cannot call createFile before the
            // namespace exists either.
            if (retries < 50) {
                setTimeout(function () { joInstallPdfPasswordWrap(retries + 1); }, 100);
            } else {
                console.error('[jOffice] PDF_PW_WRAP viewer namespace never appeared');
            }
            return;
        }
        var orig = window.AscViewer.createFile;
        window.AscViewer.createFile = function (data) {
            var f = orig.call(this, data);
            try {
                if (f && f.isNeedPassword && f.isNeedPassword() && joPdfPw != null) {
                    var pw = joPdfPw;
                    joPdfPw = null; // drop our reference first
                    window.AscViewer.setFilePassword(f, pw);
                    pw = null;
                    var still = !!(f.isNeedPassword && f.isNeedPassword());
                    joLedger('PDF_PW_ENGINE', null, { accepted: still ? 'no' : 'yes' });
                    if (still) bridge('pdfPasswordRejected', {});
                }
            } catch (e) {
                console.error('[jOffice] PDF password wrap error: '
                    + (e && e.message ? e.message : e));
            }
            return f;
        };
        window.AscViewer.__joPwWrapped = true;
    }

    // The copy bit is enforced by OUR code (Rust read the
    // bit; the editor is not trusted to): with copy off,
    // copy/cut events in this window are swallowed.
    var joClipboardGateInstalled = false;
    function joInstallClipboardGate() {
        if (joClipboardGateInstalled) return;
        if (!joPdf.perms || joPdf.perms.copy) return;
        joClipboardGateInstalled = true;
        var block = function (e) {
            e.preventDefault();
            joLedger('CLIPBOARD_BLOCKED', null, { kind: e.type });
            bridge('clipboardBlocked', { kind: e.type });
        };
        document.addEventListener('copy', block, true);
        document.addEventListener('cut', block, true);
        joLedger('CLIPBOARD_GATE', null, { armed: 'yes' });
    }

    // PE0 (cycle 5 step 3): trap the two api calls whose
    // arguments decide whether a PDF window may annotate and
    // may flush. Re-asserted on a poll like the font wrap:
    // the prototype methods are assigned when apiBase.js
    // loads and could in principle be overwritten later.
    (function installPe0Traps() {
        var asserted = {};
        setInterval(function () {
            // PDFEditorApi OVERRIDES asc_setPdfViewer and
            // asc_Print (pdf/api.js:5164/4335) without calling
            // the base — trap BOTH prototypes or the probes
            // see nothing in a PDF window.
            var protos = [];
            var base = window.AscCommon && window.AscCommon.baseEditorsApi
                && window.AscCommon.baseEditorsApi.prototype;
            if (base) protos.push(['base', base]);
            var pdfApi = (window.Asc && window.Asc.PDFEditorApi)
                || window.PDFEditorApi;
            if (pdfApi && pdfApi.prototype && pdfApi.prototype !== base) {
                protos.push(['pdf', pdfApi.prototype]);
            }
            protos.forEach(function (pair) {
            var proto = pair[1];
            ['asc_setPdfViewer', 'asc_setCanSendChanges'].forEach(function (fn) {
                var cur = proto[fn];
                if (typeof cur === 'function' && !cur.__joPe0) {
                    var orig = cur;
                    var wrapped = function (a) {
                        try {
                            joLedger('PE0_CALL', null, {
                                fn: fn, args: JSON.stringify([a])
                            });
                        } catch (e) { /* ledger only */ }
                        return orig.apply(this, arguments);
                    };
                    wrapped.__joPe0 = true;
                    wrapped.__joOrig = orig;
                    proto[fn] = wrapped;
                    if (!asserted[fn]) {
                        asserted[fn] = true;
                        console.log('[jOffice][save-ledger] PE0 trap asserted fn=' + fn);
                    }
                }
            });
            // Step 4: the print path's payload. Round 2: the
            // compiled unsaved annotations ride one of the
            // print call's arguments (base64). Log every
            // argument's shape; decode any long base64 string
            // and report its head + length (PDF_PRINT_ARG).
            var curPrint = proto['asc_Print'];
            if (typeof curPrint === 'function' && !curPrint.__joPe0) {
                var origPrint = curPrint;
                var wrappedPrint = function () {
                    try {
                        var shapes = [];
                        for (var i = 0; i < arguments.length; i++) {
                            var a = arguments[i];
                            if (typeof a === 'string' && a.length > 100) {
                                var b = joPdfAsBytes(a);
                                shapes.push('arg' + i + '=b64str:' + a.length
                                    + (b ? ' head=' + joPdfHead(b) : ''));
                                if (b) {
                                    joLedger('PDF_PRINT_ARG', null, {
                                        idx: i, bytes: b.length, head: joPdfHead(b)
                                    });
                                }
                            } else {
                                shapes.push('arg' + i + '=' + (a === null ? 'null'
                                    : typeof a === 'object' ? 'object' : String(a)));
                            }
                        }
                        joLedger('PDF_PRINT_CALL', null, { args: shapes.join(' ') });
                    } catch (e) { /* ledger only */ }
                    return origPrint.apply(this, arguments);
                };
                wrappedPrint.__joPe0 = true;
                wrappedPrint.__joOrig = origPrint;
                proto['asc_Print'] = wrappedPrint;
                if (!asserted['asc_Print']) {
                    asserted['asc_Print'] = true;
                    console.log('[jOffice][save-ledger] PE0 trap asserted fn=asc_Print');
                }
            }
            });
        }, 25);
    })();

    // Resolve the live CPDFDoc for this window's PDF, probing
    // the candidate accessors in order and logging what each
    // yields (PDF_DOC_PROBE). Returns the doc or null.
    function joPdfDoc() {
        var ref = getEditor();
        var api = ref && ref.editor;
        if (!api) return null;
        var cands = [];
        // The pinned path (pdf/api.js:105; viewer.js calls
        // Asc.editor.getPDFDoc()): the direct CPDFDoc accessor.
        try { cands.push(['api.getPDFDoc()', api.getPDFDoc && api.getPDFDoc()]); } catch (e) { cands.push(['api.getPDFDoc()', 'THROW ' + e]); }
        try {
            var rend = api.getDocumentRenderer && api.getDocumentRenderer();
            cands.push(['renderer.getPDFDoc()', rend && rend.getPDFDoc && rend.getPDFDoc()]);
        } catch (e) { cands.push(['renderer.getPDFDoc()', 'THROW ' + e]); }
        try { cands.push(['api.getDocument()', api.getDocument && api.getDocument()]); } catch (e) { cands.push(['api.getDocument()', 'THROW ' + e]); }
        try { cands.push(['api.getPDFDocument()', api.getPDFDocument && api.getPDFDocument()]); } catch (e) { cands.push(['api.getPDFDocument()', 'THROW ' + e]); }
        try { cands.push(['api.pdfDocument', api.pdfDocument]); } catch (e) { cands.push(['api.pdfDocument', 'THROW ' + e]); }
        try { cands.push(['api.doc', api.doc]); } catch (e) { cands.push(['api.doc', 'THROW ' + e]); }
        for (var i = 0; i < cands.length; i++) {
            var v = cands[i][1];
            if (v && typeof v === 'object' && v.History && v.GetPagesCount) {
                if (!joPdf.docLogged) {
                    joPdf.docLogged = true;
                    joLedger('PDF_DOC_PROBE', null, { via: cands[i][0] });
                }
                return v;
            }
        }
        if (!joPdf.docLogged) {
            joPdf.docLogged = true;
            var seen = cands.map(function (c) {
                return c[0] + '=' + (c[1] === null ? 'null'
                    : typeof c[1] === 'object' ? 'object' : String(c[1]));
            }).join(' ');
            joLedger('PDF_DOC_PROBE', null, { via: 'NONE', seen: seen });
        }
        return null;
    }

    // The editor's own annotation count (CPDFDoc.annots
    // concatenates every page's annots). Null when the doc
    // cannot be resolved — the count gate then fails closed.
    function joPdfAnnotCount() {
        var doc = joPdfDoc();
        if (!doc) return null;
        try {
            var a = doc.annots;
            if (a && typeof a.length === 'number') return a.length;
        } catch (e) { /* fall through */ }
        try {
            var n = 0, pages = doc.GetPagesCount();
            for (var i = 0; i < pages; i++) {
                var info = doc.GetPageInfo(i);
                if (info && info.annots) n += info.annots.length;
            }
            return n;
        } catch (e) { return null; }
    }

    // Stream-kind detection (cycle 5 continuation, KAO
    // round 3 §X5): viewer.Save() describes state relative
    // to the file the window OPENED. A capture taken inside
    // a save cycle is a DELTA against that base (the
    // history's delete index still sits at the last flush);
    // a capture outside one is the FULL state. KAO's
    // formula needs base = changes[0..deleteIndex] and
    // delta = changes[deleteIndex..] — but the pinned PDF
    // History (AscPDF.History) exposes GetDeleteIndex()
    // and NO GetChanges() (probed live, battery D: the
    // method is undefined; Points is the storage). The
    // journal wrap tracks the same total the delete index
    // counts (joJournal.histLen, in wire records — the
    // units agree: one note flush = 16 records and
    // GetDeleteIndex() = 16 right after it), so base =
    // deleteIndex and delta = histLen - deleteIndex.
    // Delta-kind iff both are non-empty. A delta may only
    // ever be merged into the opened file — Rust enforces
    // the target (PDF-TARGET-GATE); this tag is the
    // second, descriptive net.
    function joPdfStreamKind(doc) {
        var out = { kind: 'unknown', base: -1, delta: -1 };
        try {
            var h = doc && doc.History;
            if (!h || typeof h.GetDeleteIndex !== 'function') return out;
            var di = h.GetDeleteIndex();
            var total = (typeof joJournal !== 'undefined' && joJournal)
                ? joJournal.histLen : null;
            if (typeof di !== 'number' || typeof total !== 'number'
                || !(total > 0)) return out;
            out.base = Math.min(di, total);
            out.delta = Math.max(0, total - di);
            out.kind = (out.base > 0 && out.delta > 0)
                ? 'delta' : 'full';
        } catch (e) { /* leave unknown */ }
        return out;
    }
    window.__joPdfStreamKind = function () {
        var d = joPdfDoc();
        return d ? joPdfStreamKind(d) : null;
    };

    // Normalize a capture result to bytes: Uint8Array /
    // ArrayBuffer pass through; a string is tried as base64
    // (the compiled-changes channel in the desktop contract
    // is base64, e.g. the Print argument). Anything else is
    // not bytes — null.
    function joPdfAsBytes(v) {
        if (!v) return null;
        if (v instanceof Uint8Array) return v;
        if (v instanceof ArrayBuffer) return new Uint8Array(v);
        if (typeof v === 'string' && v.length > 8) {
            try { return _b64ToBytes(v); } catch (e) { return null; }
        }
        return null;
    }

    function joPdfHead(bytes) {
        var s = '';
        for (var i = 0; i < Math.min(4, bytes.length); i++) {
            s += String.fromCharCode(bytes[i]);
        }
        return s;
    }

    // Capture the compiled annotation changes from the live
    // PDF document (cycle 5 step 4). The desktop contract's
    // compiled form starts with the 4 bytes "%PDF" (pinned
    // core, applyCompiledChangesPdf). Candidates are probed
    // in order and every outcome is ledgered
    // (PDF_CAPTURE_PROBE) — the first candidate yielding
    // bytes wins. cb({bytes, annotCount, via}) or cb(null).
    function joPdfCaptureCompiled(cb) {
        var doc = joPdfDoc();
        if (!doc) {
            joLedger('PDF_CAPTURE_PROBE', null, { via: 'NONE', why: 'no doc' });
            cb(null);
            return;
        }
        // PS2: text still being typed in a free-text annot is
        // committed only when the active object blurs; blur
        // BEFORE capturing or the keystrokes are lost.
        try {
            if (typeof doc.BlurActiveObject === 'function') {
                doc.BlurActiveObject();
                joLedger('PDF_BLUR', null, { ok: 1 });
            } else {
                joLedger('PDF_BLUR', null, { ok: 0, why: 'absent' });
            }
        } catch (e) {
            joLedger('PDF_BLUR', null, { ok: 0, why: String(e) });
        }
        var annotCount = joPdfAnnotCount();
        var streamKind = joPdfStreamKind(doc);
        var tries = [];
        function attempt(name, fn) {
            try {
                var v = fn();
                var b = joPdfAsBytes(v);
                tries.push(name + '=' + (b ? 'bytes:' + b.length
                    : v === null || v === undefined ? 'null'
                    : typeof v));
                if (b) {
                    joLedger('PDF_CAPTURE_PROBE', null, {
                        via: name, tries: tries.join(' ')
                    });
                    cb({
                        bytes: b, annotCount: annotCount, via: name,
                        streamKind: streamKind.kind,
                        streamBase: streamKind.base,
                        streamDelta: streamKind.delta
                    });
                    return true;
                }
            } catch (e) {
                tries.push(name + '=THROW:' + String(e).slice(0, 60));
            }
            return false;
        }
        var viewer = null;
        try { viewer = doc.Viewer || null; } catch (e) { /* probe below */ }
        if (attempt('viewer.Save', function () { return viewer && viewer.Save(); })) return;
        var file = null;
        try { file = doc.GetFile && doc.GetFile(); } catch (e) { /* probe below */ }
        if (file && !joPdf.fileMethodsLogged) {
            joPdf.fileMethodsLogged = true;
            var names = [];
            try {
                for (var k in file) {
                    if (typeof file[k] === 'function') names.push(k);
                }
            } catch (e) { /* enumeration only */ }
            joLedger('PDF_FILE_METHODS', null, {
                methods: names.join(',').slice(0, 400)
            });
        }
        if (attempt('file.Save', function () { return file && file.Save(); })) return;
        if (attempt('file.save', function () { return file && file.save(); })) return;
        if (attempt('viewer.GetFile().Save', function () {
            var f = viewer && viewer.GetFile && viewer.GetFile();
            return f && f.Save && f.Save();
        })) return;
        joLedger('PDF_CAPTURE_PROBE', null, {
            via: 'NONE', tries: tries.join(' ')
        });
        cb(null);
    }

    // The PDF save send (cycle 5): mirrors the bin path's
    // dual-channel reply plumbing exactly, but the payload is
    // the compiled changes and saveArgs carries pdfChanges +
    // the editor's annotation count for Rust's count gate.
    function joPdfSendSave(seq, ctx, cap) {
        joSave.lastBinIndex = null;
        var settleSaveReply = null;
        var replyPromise = new Promise(function (resolve) {
            settleSaveReply = resolve;
        });
        joSave.replyWaiters[seq] = function (res) {
            if (settleSaveReply) {
                var s = settleSaveReply;
                settleSaveReply = null;
                delete joSave.replyWaiters[seq];
                s(res);
            }
        };
        var saveArgs = {
            saveAs: ctx.isSaveAs, fileType: ctx.fileType || 0,
            origin: ctx.origin, seq: seq,
            unmodified: ctx.unmodified,
            absorb: false,
            histIndex: null,
            printIntent: ctx.printIntent,
            printPages: ctx.printPages,
            pdfa: false,
            pdfChanges: true,
            annotCount: cap.annotCount,
            captureVia: cap.via,
            streamKind: cap.streamKind || 'unknown',
            streamBase: (cap.streamBase === undefined ? -1 : cap.streamBase),
            streamDelta: (cap.streamDelta === undefined ? -1 : cap.streamDelta)
        };
        var sendSave = function () {
            bridge('saveFile', saveArgs)
                .then(function (res) {
                    if (res && typeof res.ok !== 'undefined') {
                        if (joSave.replyWaiters[seq]) {
                            joSave.replyWaiters[seq](res);
                        }
                    } else {
                        console.log('[jOffice][save-ledger] saveFile postMessage reply seq=' + seq + ' garbled (' + JSON.stringify(res) + '); awaiting JS-channel delivery');
                    }
                })
                .catch(function (e) {
                    if (joSave.replyWaiters[seq]) {
                        joSave.replyWaiters[seq]({ ok: false, failed: true });
                    }
                });
        };
        var bytes = cap.bytes;
        var useUpload = !window.__joForceBase64
            && bytes.length > 4194304 && !!joJournal.sid;
        if (useUpload) {
            joUpload('save', String(seq), bytes, function (ok, sha) {
                if (!ok) {
                    console.error('[jOffice] pdf save upload failed seq=' + seq);
                    if (joSave.replyWaiters[seq]) {
                        joSave.replyWaiters[seq]({ ok: false, failed: true });
                    }
                    return;
                }
                joLedger('SAVE_UPLOAD', seq, { bytes: bytes.length, sha256: sha });
                saveArgs.uploadRef = seq;
                sendSave();
            });
        } else {
            saveArgs.data = _bytesToB64(bytes);
            sendSave();
        }
        replyPromise
            .then(function (res) {
                console.log('[jOffice][save-ledger] saveFile reply seq=' + seq + ' res=' + JSON.stringify(res));
                if (res && res.failed) {
                    joEndSave(seq, 'failed');
                    return;
                }
                if (res && res.cancelled) {
                    joEndSave(seq, 'cancelled');
                    return;
                }
                if (res && res.export) {
                    joEndSave(seq, 'export_ok');
                    return;
                }
                if (res && res.printed !== undefined) {
                    joEndSave(seq, 'export_ok');
                    return;
                }
                if (!res || res.ok !== true) {
                    console.error('[jOffice][save-ledger] saveFile reply seq=' + seq
                        + ' unrecognized shape; treating as failure');
                    joLedger('SAVE_REPLY_GARBLED', seq, {});
                    joEndSave(seq, 'failed');
                    return;
                }
                joSave.boundSaved = true;
                joEndSave(seq, 'bound_ok');
            })
            .catch(function (e) {
                console.error('[jOffice] LocalFileSave (pdf) failed: ' + (e && e.message ? e.message : e));
                joEndSave(seq, 'failed');
            });
    }

    // E0 instrumentation: search a byte array for the drive's
    // marker in UTF-8 and UTF-16LE forms. The marker is set by
    // the drive (window.__joE0Marker) before typing.
    function joMarkerInBytes(bytes) {
        var marker = window.__joE0Marker;
        if (!marker || !bytes) return null;
        var i, j, ok;
        // UTF-8 / ASCII form.
        outer8: for (i = 0; i + marker.length <= bytes.length; i++) {
            for (j = 0; j < marker.length; j++) {
                if (bytes[i + j] !== marker.charCodeAt(j)) continue outer8;
            }
            return 'utf8';
        }
        // UTF-16LE form.
        outer16: for (i = 0; i + marker.length * 2 <= bytes.length; i++) {
            for (j = 0; j < marker.length; j++) {
                if (bytes[i + j * 2] !== marker.charCodeAt(j)
                    || bytes[i + j * 2 + 1] !== 0) continue outer16;
            }
            return 'utf16le';
        }
        return null;
    }
    window.__joMarkerInBytes = joMarkerInBytes;
    window.__joUpload = joUpload;
    window.__joGetFile3Snapshot = function (where) {
        // Async form (cycle 4, B3-109/S0-109 fix): the
        // ledger lines are the deliverable; the eval caller
        // just kicks it off.
        joGetFile3SnapshotAsync(where || 'eval', function () { /* ledger only */ });
        return 'kicked';
    };

    // Parse an asc_nativeGetFile3 header:
    // "<Signature>;v<ver>;<len>;" → len (the data byte count the
    // header vouches for). Returns null on a malformed header
    // (GETFILE3_HEADER in the ledger at the call site).
    function joParseGetFile3Header(header) {
        if (typeof header !== 'string') return null;
        var parts = header.split(';');
        if (parts.length < 4 || parts[1].charAt(0) !== 'v') return null;
        var len = parseInt(parts[2], 10);
        if (isNaN(len) || len < 0) return null;
        return len;
    }

    // Raw capture for a snapshot: asc_nativeGetFile3 + header
    // validation, WITHOUT turning the payload into bytes. The
    // capture is synchronous on purpose: at checkpoint time
    // the serialized state must be the document state at that
    // exact instant. Turning the payload into bytes is a
    // separate step because in 9.3.8 the payload is BASE64
    // TEXT of the writer's memory (Write(true, true) →
    // GetBase64Memory), and at large sizes (a 219 MB memory
    // is ~292M chars) the decode must be sliced — see
    // joGetFile3SnapshotAsync.
    function joGetFile3Raw(where) {
        var ref = getEditor();
        if (!ref.editor || typeof ref.editor.asc_nativeGetFile3 !== 'function') {
            joLedger('GETFILE3_MISSING', null, { where: where });
            return null;
        }
        var t0 = Date.now();
        var res;
        try {
            res = ref.editor.asc_nativeGetFile3();
        } catch (e) {
            joLedger('GETFILE3_THROW', null, { where: where, err: String(e) });
            return null;
        }
        var ms = Date.now() - t0;
        if (!res || typeof res !== 'object') {
            joLedger('GETFILE3_BADSHAPE', null, { where: where });
            return null;
        }
        var len = joParseGetFile3Header(res.header);
        if (len === null) {
            joLedger('GETFILE3_HEADER', null, { where: where, header: String(res.header).slice(0, 80) });
            return null;
        }
        return { header: res.header, len: len, ms: ms, data: res.data, ew: ref.ew };
    }

    // Validate decoded bytes against the header length, emit
    // the GETFILE3_MS ledger + the E0 marker check, and hand
    // back the snapshot object (or null, with the failure in
    // the ledger).
    function joSnapshotFinish(where, raw, bytes) {
        if (!bytes) {
            joLedger('GETFILE3_BADSHAPE', null, { where: where, dataType: typeof raw.data });
            return null;
        }
        if (bytes.length !== raw.len) {
            // The header length field is the writer's total
            // memory position: it must equal the memory length.
            joLedger('GETFILE3_SHORT', null, { where: where, len: raw.len, have: bytes.length });
            return null;
        }
        joLedger('GETFILE3_MS', null, { where: where, ms: raw.ms, len: raw.len, raw: bytes.length });
        if (window.__joE0Marker) {
            var hit = joMarkerInBytes(bytes);
            var histIdx = null;
            try {
                var w = raw.ew || window;
                if (w.AscCommon && w.AscCommon.History) histIdx = w.AscCommon.History.Index;
            } catch (e) { /* ledger input only */ }
            joLedger('BIN_HAS_MARKER', null, {
                where: where, hit: hit || 'none', histIdx: histIdx, len: raw.len
            });
        }
        return { header: raw.header, len: raw.len, bytes: bytes };
    }

    function joRawBytesSync(raw) {
        var data = raw.data;
        if (data instanceof Uint8Array) return data;
        if (typeof data === 'string') {
            // Decode the base64 text to the raw bytes.
            // (Treating the text as raw bytes made x2t salvage
            // the UNEDITED document from the truncated stream
            // and silently drop the edit — caught by Bp in
            // cycle 4.)
            try { return _b64ToBytes(data); } catch (e) { return null; }
        }
        if (data && data.buffer) {
            return new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength);
        }
        return null;
    }

    // Snapshot the live document via asc_nativeGetFile3 WITHOUT
    // converting or writing (E0 mode M2 + S0 timing + epoch
    // checkpoints). Returns {header, len, bytes} where bytes is
    // the writer's memory EXACTLY as serialized — Bp parity
    // (cycle 4, settled empirically): the memory begins with
    // the in-data header ("DOCY;v10;0;") and IS the whole
    // Editor.bin; prepending the outer header guts the
    // document, and the getFile (v5, header + base64) form
    // converts to a byte-identical result.
    // SYNCHRONOUS decode; kept for callers that need the bytes
    // in the same turn. The save / S0 / checkpoint paths use
    // the async form below.
    function joGetFile3Snapshot(where) {
        var raw = joGetFile3Raw(where);
        if (!raw) return null;
        return joSnapshotFinish(where, raw, joRawBytesSync(raw));
    }

    // Async form: the capture is the same synchronous
    // asc_nativeGetFile3 call (the document state is exact at
    // that instant), but a base64-string payload is decoded in
    // setTimeout slices so the main thread keeps servicing
    // IPC. The synchronous decode blocked the main thread for
    // minutes at 109.5 MB and WebKit's crashAfter10Seconds
    // watchdog aborted the web process mid-save (cycle 4,
    // B3-109/S0-109: two SIGABRT core dumps). done(snap|null)
    // mirrors joGetFile3Snapshot's return.
    function joGetFile3SnapshotAsync(where, done) {
        var raw = joGetFile3Raw(where);
        if (!raw) { done(null); return; }
        if (typeof raw.data !== 'string') {
            done(joSnapshotFinish(where, raw, joRawBytesSync(raw)));
            return;
        }
        _b64ToBytesSliced(raw.data, function (bytes) {
            done(joSnapshotFinish(where, raw, bytes));
        });
    }

    // The Editor.bin from a getFile3 snapshot IS the snapshot
    // bytes (Bp-proven, cycle 4): the writer's memory starts
    // with its own in-data header and converts byte-identically
    // to the getFile form. Kept as a named step so the save
    // path reads clearly.
    function joAssembleBin(snap) {
        return snap.bytes;
    }

    // E0 mode M2: the flush IS a snapshot (timed, marker-checked,
    // never written anywhere). Async decode: a flush must never
    // stall the main thread at large sizes.
    function joE0Snapshot(where) {
        joGetFile3SnapshotAsync(where, function () { /* ledger only */ });
    }

    // -----------------------------------------------------------
    // joOpen (cycle 4, round 2 §D): ONE entry point for every
    // document open — launcher/--file (LocalStartOpen), the
    // editor's Open dialog (LocalFileOpen), and crash recovery.
    // The Rust convert reply carries either base64 (small bins)
    // or a session URL + exact byte count (large bins); the URL
    // path fetches same-origin via XHR (WebKitGTK gives
    // custom-scheme pages an empty Origin cross-origin, so the
    // session is served from the page's OWN host), parks the
    // buffer in the one-shot GetOpenedFile slot, and EndLoads
    // with binary_content://editor.bin. Fallback order: XHR →
    // fetch → base64 (only when the reply carries it) → refuse
    // with a logged size message. joOpen resolves only when the
    // buffer's byteLength equals the expected count.
    // -----------------------------------------------------------
    var joOpenedSlot = { buf: null };
    window.__joOpenedSlot = joOpenedSlot;

    function joOpenApplySession(res) {
        if (res.sid) {
            joJournal.sid = res.sid;
            joJournal.enabled = !!res.journal;
            joJournal.epoch = res.epoch || 0;
            joJournal.base = res.base || 0;
            joJournal.seq = res.seq || 0;
            joJournal.histLen = res.base || 0;
            joJournal.openChanges = res.openChanges || 0;
            joJournal.unsavedFloor = res.openChanges || 0;
        }
        if (res.recovery) {
            joJournal.pendingRecovery = res.recovery;
        }
        // Cycle 5: a rawPdf open reply marks this window as a
        // PDF window for the journal branch and the save
        // capture. Any other open clears it.
        joPdf.active = !!res.rawPdf;
        joPdf.mode = res.pdfMode || null;
        joPdf.docLogged = false;
        joPdf.degradedLogged = false;
        if (res.rawPdf) {
            joLedger('PDF_OPEN_STATE', null, {
                mode: joPdf.mode || '?', annotate: !!res.pdfAnnotate
            });
        }
        // Protected cycle: Rust hands a protected PDF's
        // password through the open reply (a data channel —
        // never a constructed script string). The createFile
        // wrap consumes it ONCE and drops the reference; it
        // is never logged. Permission bits (read by Rust from
        // /Encrypt) arm the clipboard gate when copy is off.
        joPdf.protected = !!res.protected;
        joProtectedDoc = !!res.protected;
        joPdf.perms = res.pdfPerms || null;
        if (joPdf.perms) joInstallClipboardGate();
        // An EMPTY string is a real credential (owner-restricted
        // PDFs whose user password is empty); only an absent
        // field means "no password to feed". Collapsing "" to
        // null left the engine in isNeedPassword() forever and
        // the page never rendered (PW-R2 leg 2 drive).
        joPdfPw = (typeof res.pdfPassword === 'string')
            ? res.pdfPassword : null;
        if (joPdfPw != null) joInstallPdfPasswordWrap(0);
    }

    function joEndLoadWith(name, data, len) {
        // CR1 (round 2 §D): the crypto path must not be armed
        // when EndLoad runs — in the pinned 9.3.8 tree nothing
        // assigns AscCommon.CryptoProvider at all, and our
        // CryptoMode is 0. Asserted here, at the one place
        // every open passes through.
        var cp = null;
        try { cp = window.AscCommon ? window.AscCommon.CryptoProvider : null; } catch (e) { /* log only */ }
        joLedger('CRYPTO_STATE', null, {
            provider: String(cp), mode: window.AscDesktopEditor
                ? window.AscDesktopEditor.CryptoMode : '?'
        });
        if (typeof window.DesktopOfflineAppDocumentEndLoad === 'function') {
            window.DesktopOfflineAppDocumentEndLoad(name || '', data, len);
            return true;
        }
        return false;
    }

    function joOpenWithBin(res, name, resetHandshake) {
        joOpenApplySession(res);
        if (resetHandshake) joSave.handshakeSent = false;
        if (res.binUrl) {
            joOpenStage.stage = 'fetch_bin';
            var expected = res.binBytes || 0;
            console.log('[jOffice][measure] OPEN_STAGE fetch_begin t=' + Date.now()
                + ' url=' + res.binUrl + ' expect=' + expected);
            var xhr = new XMLHttpRequest();
            xhr.open('GET', res.binUrl, true);
            xhr.responseType = 'arraybuffer';
            xhr.onload = function () {
                if (xhr.status !== 200 || !xhr.response
                    || xhr.response.byteLength !== expected) {
                    console.error('[jOffice] OPEN_BIN_FAIL status=' + xhr.status
                        + ' got=' + (xhr.response ? xhr.response.byteLength : -1)
                        + ' expect=' + expected);
                    joOpenFetchFallback(res, name);
                    return;
                }
                joOpenedSlot.buf = xhr.response;
                joOpenStage.stage = 'received';
                console.log('[jOffice][measure] OPEN_STAGE received t=' + Date.now()
                    + ' bin_bytes=' + xhr.response.byteLength + ' via=url');
                joOpenStage.stage = 'endload_in';
                if (!joEndLoadWith(name, 'binary_content://editor.bin', 0)) {
                    console.error('[jOffice] EndLoad missing on the URL open path');
                    return;
                }
                joOpenStage.stage = 'endload_out';
                console.log('[jOffice][measure] OPEN_STAGE endload_done t=' + Date.now());
                setTimeout(joHandshake, 300);
            };
            xhr.onerror = function () {
                console.error('[jOffice] OPEN_BIN_XHR_ERROR url=' + res.binUrl);
                joOpenFetchFallback(res, name);
            };
            xhr.send(null);
            return;
        }
        if (res.data) {
            var bin = _b64ToBytes(res.data);
            joOpenStage.stage = 'received';
            console.log('[jOffice][measure] OPEN_STAGE received t=' + Date.now()
                + ' b64_chars=' + res.data.length);
            joOpenStage.stage = 'decoded';
            console.log('[jOffice][measure] OPEN_STAGE decoded t=' + Date.now()
                + ' bin_bytes=' + bin.length);
            joOpenStage.stage = 'endload_in';
            if (!joEndLoadWith(name, res.data, bin.length)) {
                console.warn('[jOffice] DesktopOfflineAppDocumentEndLoad MISSING, using fallback openDocument');
                var ref = getEditor();
                if (!ref.editor) throw new Error('no editor API');
                var file = new ref.ew.AscCommon.OpenFileResult();
                file.data = bin;
                file.bSerFormat = true;
                file.url = name || '';
                ref.editor.openDocument(file);
            }
            joOpenStage.stage = 'endload_out';
            console.log('[jOffice][measure] OPEN_STAGE endload_done t=' + Date.now());
            setTimeout(joHandshake, 300);
            return;
        }
        console.error('[jOffice] joOpen: reply carried neither binUrl nor data');
    }

    function joOpenFetchFallback(res, name) {
        // Fallback order (round 2 §D): fetch, then base64 when
        // the reply carries it, then refuse with a size message.
        if (typeof fetch === 'function' && res.binUrl) {
            fetch(res.binUrl).then(function (r) { return r.arrayBuffer(); })
                .then(function (buf) {
                    if (buf.byteLength !== (res.binBytes || 0)) {
                        throw new Error('fetch size mismatch');
                    }
                    joOpenedSlot.buf = buf;
                    joOpenStage.stage = 'endload_in';
                    if (!joEndLoadWith(name, 'binary_content://editor.bin', 0)) {
                        console.error('[jOffice] EndLoad missing on the fetch open path');
                        return;
                    }
                    joOpenStage.stage = 'endload_out';
                    setTimeout(joHandshake, 300);
                })
                .catch(function (e) {
                    if (res.data) {
                        joOpenWithBin({ data: res.data, sid: res.sid, journal: res.journal }, name, false);
                    } else {
                        console.error('[jOffice] OPEN_REFUSED bytes=' + (res.binBytes || 0)
                            + ' (URL transports failed, no base64 fallback carried)');
                    }
                });
            return;
        }
        if (res.data) {
            joOpenWithBin({ data: res.data, sid: res.sid, journal: res.journal }, name, false);
            return;
        }
        console.error('[jOffice] OPEN_REFUSED bytes=' + (res.binBytes || 0));
    }

    // Dual-channel save-reply delivery (cycle 3): Rust evaluates
    // this in the top frame; the top frame forwards to the
    // editor frame, whose copy settles the seq's waiter.
    window.__joDeliverSaveReply = function (seq, res) {
        if (joSave.replyWaiters[seq]) {
            joSave.replyWaiters[seq](res);
            return true;
        }
        var ref = getEditor();
        if (ref.ew && ref.ew !== window && ref.ew.__joDeliverSaveReply) {
            return ref.ew.__joDeliverSaveReply(seq, res);
        }
        return false;
    };

    // Dual-channel OPEN-reply delivery (cycle 4): the deferred-
    // reply garbling first found on save replies in cycle 3
    // also hits open-family replies — the postMessage promise
    // intermittently resolves with an EMPTY object when Rust
    // answers from the x2t completion callback, so a 240 MB
    // open whose conversion had succeeded was reported as
    // "convertFile returned no data" and the document never
    // loaded (B1). Open-family calls therefore carry an
    // openSeq and settle on the first WELL-FORMED reply from
    // either channel; Rust also evaluates
    // window.__joDeliverOpenReply(seq, res) (top frame, which
    // forwards to the editor frame, exactly as save replies).
    var joOpenReply = { seq: 0, waiters: {} };
    window.__joDeliverOpenReply = function (seq, res) {
        if (joOpenReply.waiters[seq]) {
            joOpenReply.waiters[seq](res);
            return true;
        }
        var ref = getEditor();
        if (ref.ew && ref.ew !== window && ref.ew.__joDeliverOpenReply) {
            return ref.ew.__joDeliverOpenReply(seq, res);
        }
        return false;
    };

    function joOpenWellFormed(res) {
        return !!(res && (res.data || res.binUrl || res.error));
    }

    // bridge() for open-family commands whose replies are
    // deferred (x2t conversions, native dialogs): resolves
    // with the first well-formed reply from the postMessage
    // channel or the JS channel; a garbled postMessage reply
    // is logged and ignored, never settled. Rejects if the
    // postMessage promise rejects (an explicit Rust error is
    // a final answer).
    function joBridgeOpen(cmd, args, wellFormed) {
        return new Promise(function (resolve, reject) {
            var seq = ++joOpenReply.seq;
            var settled = false;
            var settle = function (res) {
                if (settled) return;
                settled = true;
                delete joOpenReply.waiters[seq];
                resolve(res);
            };
            joOpenReply.waiters[seq] = function (res) {
                if (wellFormed(res)) settle(res);
            };
            args = args || {};
            args.openSeq = seq;
            bridge(cmd, args).then(function (res) {
                if (wellFormed(res)) {
                    settle(res);
                } else {
                    console.log('[jOffice] ' + cmd + ' postMessage reply garbled ('
                        + JSON.stringify(res) + '); awaiting JS-channel delivery seq=' + seq);
                    joLedger('OPEN_REPLY_GARBLED', null, { cmd: cmd, seq: seq });
                }
            }).catch(function (e) {
                if (!settled) {
                    settled = true;
                    delete joOpenReply.waiters[seq];
                    reject(e);
                }
            });
        });
    }

    // Inserted-image bookkeeping: dialog handle -> {name,
    // localName}; localName -> info. LocalFileGetImageUrl must
    // answer SYNCHRONOUSLY, so the name is chosen here and the
    // copy is fired off to Rust in the background; the scheme
    // handler holds docmedia requests until the copy lands
    // (Kagi round 2 restatement 5a).
    var joImages = { byHandle: {}, byName: {}, counter: 0 };
    window.__joImages = joImages;

    // FNV-1a 64-bit over UTF-16 code units (BigInt keeps it
    // exact) — build identity for the SDK_DRIFT check. The
    // pinned values are bootstrapped from the live editor's own
    // computation, so this only has to be stable, not match any
    // other implementation.
    function joFnv(str) {
        var h = 0xcbf29ce484222325n;
        var prime = 0x100000001b3n;
        var mask = 0xffffffffffffffffn;
        for (var i = 0; i < str.length; i++) {
            h ^= BigInt(str.charCodeAt(i));
            h = (h * prime) & mask;
        }
        return h.toString(16).padStart(16, '0');
    }

    // The save/dirty ledger (round 2 §F): every save-pipeline
    // event with the history indices and canSave, so a drive log
    // can reconstruct exactly what Rust was told vs what the
    // editor believed. Rust logs its mirror on the same events.
    function joLedger(event, seq, extra) {
        var ref = getEditor();
        var ed = ref.editor;
        var hist = null;
        try {
            var w = ref.ew || window;
            hist = w.AscCommon && w.AscCommon.History;
        } catch (e) { /* frame gone */ }
        var parts = ['[jOffice][save-ledger]', event, 'seq=' + (seq == null ? '-' : seq)];
        if (hist) {
            parts.push('histIdx=' + hist.Index);
            parts.push('userSavedIdx=' + hist.UserSavedIndex);
            parts.push('savedIdx=' + hist.SavedIndex);
        }
        if (ed) {
            parts.push('lastUserSavedIdx=' + ed.LastUserSavedIndex);
            parts.push('canSave=' + ed.canSave);
        }
        if (extra) {
            for (var k in extra) parts.push(k + '=' + extra[k]);
        }
        console.log(parts.join(' '));
    }

    // Compare the value the push channel last delivered against
    // a direct query right now. A disagreement is a finding
    // (logged, counted in drives) — the Cell canSave suppression
    // and cell-editing state are the expected explainable causes.
    function joCheckDisagree(where) {
        var ref = getEditor();
        if (!ref.editor || typeof ref.editor.isDocumentModified !== 'function') return;
        var direct;
        try { direct = !!ref.editor.isDocumentModified(); } catch (e) { return; }
        var pushed = joSave.lastPushedModified;
        if (pushed !== null && direct !== pushed) {
            console.log('[jOffice][save-ledger] MODIFIED_DISAGREE at=' + where
                + ' pushed=' + pushed + ' direct=' + direct
                + ' cellEditing=' + joSave.cellEditing
                + ' overlay=' + !!joSave.overlayForced);
        }
        if (joPdf.active) {
            // Cycle 5 step 9: the oDoc.History mirror. A PDF's
            // edits live in CPDFDoc.History (an AscPDF.History),
            // not only in the co-api history the ledger prints;
            // "unsaved" there means Index has moved past the
            // saved point. A mirror that disagrees with the
            // direct query is a PD1 finding, logged with the
            // same MODIFIED_DISAGREE prefix the drives count.
            var doc = joPdfDoc();
            var h = doc && doc.History;
            if (h && h.Index !== null && h.Index !== undefined
                && h.SavedIndex !== null && h.SavedIndex !== undefined) {
                var mirror = h.Index !== h.SavedIndex;
                if (mirror !== direct) {
                    console.log('[jOffice][save-ledger] MODIFIED_DISAGREE at=pdf-mirror-' + where
                        + ' mirror=' + mirror + ' direct=' + direct
                        + ' histIdx=' + h.Index + ' savedIdx=' + h.SavedIndex
                        + ' userSavedIdx=' + h.UserSavedIndex);
                }
            }
        }
    }

    // The one-EndSave-per-StartSave guard (round 2 §C). Every
    // save outcome funnels through here, keyed by sequence
    // number: a second call for the same seq logs ENDSAVE_DUP
    // and does nothing (a duplicate EndSave restores from an
    // already-cleared LastUserSavedIndex and corrupts the
    // saved-point index). Outcome → code mapping is the whole
    // contract: bound_ok 0 (the bound file was saved);
    // export_ok / cancelled / failed 1 (it was not; the saved
    // point is restored and the document stays dirty). Code 2
    // is never used — failures surface through the shell's
    // native presenter, not sdkjs's conversion-error event.
    var JO_ENDSAVE_CODE = { bound_ok: 0, export_ok: 1, cancelled: 1, failed: 1 };
    function joEndSave(seq, outcome) {
        if (seq !== joSave.pendingSeq) {
            console.log('[jOffice][save-ledger] ENDSAVE_DUP seq=' + seq
                + ' outcome=' + outcome + ' pending=' + joSave.pendingSeq);
            return;
        }
        joSave.pendingSeq = null;
        if (joSave.absorbSeq === seq && joSave.absorbResolve) {
            var absorbRes = joSave.absorbResolve;
            joSave.absorbSeq = null;
            joSave.absorbResolve = null;
            absorbRes();
        }
        // Cycle 4 alarm (round 2 §B, never-cut): an EndSave for
        // a cycle sdkjs never opened. StartSave sets
        // startSaveOpen; a legitimate EndSave consumes it. In
        // journal mode flushes send no EndSave at all, so any
        // hit here is a genuine anomaly; in bound/m0 mode the
        // flush's invented EndSave trips it by design — the
        // count is the measure of that deviation.
        if (!joSave.startSaveOpen) {
            joLedger('ENDSAVE_UNCYCLED', seq, { outcome: outcome });
        }
        joSave.startSaveOpen = false;
        var code = JO_ENDSAVE_CODE[outcome];
        // bin_index assertion (cycle 3, round 2 §B): the bin
        // this EndSave(0) vouches for was captured at
        // History.Index == lastBinIndex; if UserSavedIndex has
        // since moved, the bytes and the bookkeeping disagree —
        // downgrade to a failure EndSave rather than lie.
        if (code === 0 && joSave.lastBinIndex !== null && joSave.lastBinSeq === seq) {
            try {
                var usi = window.AscCommon && window.AscCommon.History
                    ? window.AscCommon.History.UserSavedIndex : null;
                if (usi !== null && usi !== joSave.lastBinIndex) {
                    joLedger('SAVE_INDEX_MISMATCH', seq, {
                        binIndex: joSave.lastBinIndex, userSavedIndex: usi
                    });
                    code = 1;
                }
            } catch (e) { /* assertion is best-effort */ }
        }
        if (outcome === 'bound_ok') {
            // The journal's unsaved tail is now in the bound
            // file (cycle 4): the recovery floor clears, and
            // Rust records last_bound_save_index from the
            // saveFile args (histIndex) in the manifest.
            joJournal.unsavedFloor = 0;
            joJournal.openChanges = 0;
            // Item-scale baseline for the unmodified gate: the
            // gesture's own send updated histLen synchronously
            // before the snapshot, so histLen here is exactly
            // the item count the bound file now holds.
            joJournal.boundHistLen = joJournal.histLen || 0;
        }
        var ref = getEditor();
        var w = ref.ew || window;
        joLedger('endsave', seq, { outcome: outcome, code: code });
        if (typeof w.DesktopOfflineAppDocumentEndSave === 'function') {
            w.DesktopOfflineAppDocumentEndSave(code);
        } else {
            console.error('[jOffice] DesktopOfflineAppDocumentEndSave MISSING at EndSave time');
        }
        joCheckDisagree('endsave');
        // The cycle closes AFTER the real EndSave ran (its
        // LastUserSavedIndex clear is the one legitimate
        // mid-cycle write). Deferred requests re-run from here.
        joOnCycleEnd(code === 0);
    }

    // The modified handshake (round 2 §B): right after EndLoad,
    // tell Rust the push channel works, what a direct query says
    // right now, and the build identity of this editor's
    // EndSave implementation (SDK_DRIFT input). Also attach the
    // cell-editor hook while the editor object is at hand.
    function joHandshake() {
        if (joSave.handshakeSent) return;
        joSave.handshakeSent = true;
        joOpenStage.stage = 'ready';
        console.log('[jOffice][measure] OPEN_STAGE ready t=' + Date.now());
        var ref = getEditor();
        var direct = null;
        try {
            if (ref.editor && typeof ref.editor.isDocumentModified === 'function') {
                direct = !!ref.editor.isDocumentModified();
            }
        } catch (e) { /* leave null */ }
        var endSaveHash = null;
        try {
            var f = (ref.ew || window).DesktopOfflineAppDocumentEndSave;
            if (typeof f === 'function') endSaveHash = joFnv(f.toString());
        } catch (e) { /* leave null */ }
        console.log('[jOffice][save-ledger] modified_ready direct=' + direct
            + ' endSaveHash=' + endSaveHash + ' t=' + Date.now());
        bridge('modifiedReady', { value: direct, endSaveHash: endSaveHash })
            .catch(function () { /* log-only channel */ });
        joAttachEditorHooks();
        // Crash recovery replay (cycle 4): the folded journal
        // is applied over the seed, then an immediate
        // checkpoint makes the recovered state the new epoch's
        // seed. Runs once, right after the handshake.
        if (joJournal.pendingRecovery) {
            var rec = joJournal.pendingRecovery;
            joJournal.pendingRecovery = null;
            joApplyRecovery(rec);
        }
    }

    // Apply a recovered session's folded journal (round 1 §3).
    // The window stays bound to the original path, dirty, with
    // LocalFileGetSaved false and the open-changes count equal
    // to the folded list length; Rust shows the banner.
    function joApplyRecovery(rec) {
        var folded = rec.folded || [];
        joJournal.replaying = true;
        try {
            if (typeof window.DesktopOfflineAppDocumentApplyChanges === 'function') {
                window.DesktopOfflineAppDocumentApplyChanges(folded);
                joLedger('RECOVERY_APPLIED', null, { n: folded.length, gap: !!rec.gap });
            } else {
                joLedger('RECOVERY_APPLY_MISSING', null, {});
            }
        } catch (e) {
            joLedger('RECOVERY_APPLY_THROW', null, { err: String(e) });
        }
        joJournal.replaying = false;
        joSave.boundSaved = false;
        joJournal.openChanges = folded.length;
        joJournal.unsavedFloor = folded.length;
        joJournal.histLen = folded.length;
        bridge('recoveryOpened', {
            secret: window.__joSecret || '',
            sid: joJournal.sid, gap: !!rec.gap, through: rec.through || null,
            count: folded.length
        }).catch(function () { /* banner is best-effort */ });
        // The checkpoint's base must count everything the new
        // seed embodies: the recovered epoch's own base (the
        // seed the window opened from) PLUS the folded records
        // applied on top. Passing folded.length alone reset
        // Rust's base to the fold size — with an empty fold
        // (crash after a checkpoint) unsaved() fell to 0 and
        // the §6a unedited no-op ate the recovered save
        // (cycle 4, K6 follow-up).
        joJournalCheckpoint('recovery', joJournal.base + folded.length);
    }

    // Cell-editor open/close forwarding (round 2 §B): a cell
    // being edited counts as dirty even though no history point
    // exists yet. asc_onEditCell states: editEnd = 0, anything
    // else is an editing state (cell/apiDefines.js, verified
    // against the pinned tree).
    function joAttachEditorHooks() {
        var ref = getEditor();
        if (!ref.editor || typeof ref.editor.asc_registerCallback !== 'function') return;
        try {
            ref.editor.asc_registerCallback('asc_onEditCell', function (state) {
                var editing = (state !== 0 && state !== undefined && state !== null);
                if (editing !== joSave.cellEditing) {
                    joSave.cellEditing = editing;
                    joLedger('celledit', null, { editing: editing, state: state });
                    bridge('cellEditState', { editing: editing }).catch(function () { });
                }
            });
        } catch (e) { /* word/slide editors have no cell editor */ }
    }

    // StartSave wrap (round 2 §B): the {started} flag. StartSave
    // is called from INSIDE asc_Save, so if a save actually
    // begins, the flag flips synchronously during the asc_Save
    // call — that is what lets the close path distinguish "Save
    // chosen on a clean document" (asc_Save returns early,
    // started stays false, close proceeds) from a real save in
    // flight. Re-asserted on a poll, like the font wrap: the
    // Local api loads after this shim and defines the original.
    (function installStartSaveWrap() {
        setInterval(function () {
            if (typeof window.DesktopOfflineAppDocumentStartSave === 'function'
                && !window.DesktopOfflineAppDocumentStartSave.__joWrapped) {
                var orig = window.DesktopOfflineAppDocumentStartSave;
                var wrapped = function (isSaveAs, password, isForce, docinfo, options) {
                    joSave.started = true;
                    joSave.startSaveSeen = true;
                    joSave.startSaveOpen = true;
                    // A save cycle opens (cycle 3): from here to
                    // EndSave, asc_Save requests are serialized
                    // by the wrapper above, never re-entered.
                    joSave.cycle = { t: Date.now(), orphanLogged: false };
                    joLedger('startsave', joSave.pendingSeq, { isSaveAs: !!isSaveAs });
                    return orig.apply(this, arguments);
                };
                wrapped.__joWrapped = true;
                window.DesktopOfflineAppDocumentStartSave = wrapped;
                console.log('[jOffice][save-ledger] StartSave wrap asserted');
            }
        }, 25);
    })();

    // -----------------------------------------------------------
    // Save serialization (marathon 2 cycle 3, KAO round 2 §A —
    // the cycle-2 CSV/TXT failure fix). Verified against the
    // pinned 9.3.8 sources: all three editors define
    // asc_Save(isNoUserSave, isSaveAs, isResaveAttack, options),
    // and every asc_Save with IsUserSave writes
    // editor.LastUserSavedIndex = History.UserSavedIndex BEFORE
    // the canSave gate. sdkjs holds ONE LastUserSavedIndex slot
    // and EndSave(error) restores UserSavedIndex from it — so a
    // second asc_Save landing while a save cycle is open
    // silently moves the restore point (the flaw in round 1's
    // EndSave fan-out, withdrawn in round 2). The discipline:
    //   - same-gesture requests are gated for one macrotask and
    //     only the best-ranked runs (RANK flush 0 < user 1 <
    //     saveas 2) — the cycle-2 collision was a flush and a
    //     user save arriving in the same gesture;
    //   - during an open cycle, requests NEVER touch sdkjs:
    //     they land in a single best-ranked deferred slot and
    //     are re-run (saveas always; user/flush only when the
    //     cycle ended ok AND the document still has changes) or
    //     dropped with a SAVE_DROPPED ledger line;
    //   - a property trap on editor.LastUserSavedIndex raises
    //     LUSI_WRITE_IN_CYCLE on any mid-cycle write that is not
    //     EndSave's own clear (undefined);
    //   - the bin_index assertion: History.Index captured when
    //     the bin bytes are taken must equal History.
    //     UserSavedIndex when a bound_ok EndSave is delivered,
    //     else the EndSave is downgraded to code 1.
    // Reset_SavedIndex timing (round 2's open check, answered
    // from the pinned source): in Local mode askSaveChanges
    // invokes its callback SYNCHRONOUSLY, so History.
    // Reset_SavedIndex(IsUserSave) runs INSIDE asc_Save, before
    // StartSave — UserSavedIndex has already advanced when the
    // bin is captured, which is what makes the assertion
    // meaningful for the async paths it guards.
    // -----------------------------------------------------------
    joSave.deferred = null;   // { ed, orig, args, kind, rank }
    joSave.gateTimer = null;
    joSave.gateList = null;
    joSave.cycle = null;      // { t, orphanLogged } StartSave..EndSave
    joSave.lastBinIndex = null;
    joSave.lastBinSeq = null;
    joSave.trailingFlush = false;
    joSave.gesture = false;
    joSave.gestureTimer = null;
    joSave.heldFlush = null;
    joSave.absorbRunning = false;
    joSave.absorbPending = false;
    joSave.absorbNext = false;
    joSave.absorbSeq = null;
    joSave.absorbResolve = null;
    joSave.absorbPromise = null;
    joSave.flushTimer = null;
    joSave.printPages = null;
    // Open-stage breadcrumbs (cycle 3, step 5).
    var joOpenStage = { stage: 'idle', t0: 0, beat: null };
    window.__joOpenStage = joOpenStage;

    function joSaveKind(args) {
        if (args[1]) return 'saveas';
        if (args[3] && (args[3].fileType !== undefined || args[3].isNaturalDownload)) return 'saveas';
        if (args[0] === true) return 'flush';
        return 'user';
    }
    var JO_SAVE_RANK = { flush: 0, user: 1, saveas: 2 };

    function joSerializedSave(ed, orig, args) {
        var kind = joSaveKind(args);
        var req = { ed: ed, orig: orig, args: args, kind: kind, rank: JO_SAVE_RANK[kind] };
        if (joSave.cycle) {
            if (!joSave.deferred || req.rank >= joSave.deferred.rank) {
                joSave.deferred = req;
            }
            joLedger('SAVE_DEFERRED', null, { kind: kind, kept: joSave.deferred.kind });
            return;
        }
        if (!joSave.gateList) joSave.gateList = [];
        joSave.gateList.push(req);
        if (joSave.gateTimer === null) {
            joSave.gateTimer = setTimeout(function () {
                joSave.gateTimer = null;
                var list = joSave.gateList;
                joSave.gateList = null;
                if (!list || !list.length) return;
                var best = list[0];
                for (var i = 1; i < list.length; i++) {
                    if (list[i].rank >= best.rank) best = list[i];
                }
                if (list.length > 1) {
                    joLedger('SAVE_PICK', null, {
                        picked: best.kind,
                        from: list.map(function (r) { return r.kind; }).join('+')
                    });
                }
                // Gesture flag (cycle 3, S1′): askSaveChanges
                // flushes changes SYNCHRONOUSLY inside asc_Save,
                // before StartSave opens the cycle and before
                // the gesture's own LocalFileSave exists — the
                // debounce/fold checks keyed on pendingSeq and
                // cycle both miss it, and the flush's delegated
                // save wrote the pending edit into the ORIGINAL
                // during a Save As (the cycle-2 S1 leak). While
                // a gesture is being applied, LocalFileSaveChanges
                // folds: the changes are inside the bin this
                // gesture is about to capture.
                joSave.gesture = true;
                if (joSave.gestureTimer !== null) clearTimeout(joSave.gestureTimer);
                joSave.gestureTimer = setTimeout(function () {
                    joSave.gestureTimer = null;
                    joSave.gesture = false;
                }, 15000);
                best.orig.apply(best.ed, best.args);
            }, 0);
        }
    }

    function joOnCycleEnd(ok) {
        joSave.cycle = null;
        var d = joSave.deferred;
        joSave.deferred = null;
        if (!d) return;
        if (d.kind === 'saveas') {
            joLedger('SAVE_RERUN', null, { kind: d.kind });
            joSerializedSave(d.ed, d.orig, d.args);
            return;
        }
        var dirty = true;
        try {
            if (typeof d.ed.Have_Changes === 'function') dirty = !!d.ed.Have_Changes();
            else if (typeof d.ed.isDocumentModified === 'function') dirty = !!d.ed.isDocumentModified();
        } catch (e) { /* keep the permissive default */ }
        if (ok && dirty) {
            joLedger('SAVE_RERUN', null, { kind: d.kind });
            joSerializedSave(d.ed, d.orig, d.args);
        } else {
            joLedger('SAVE_DROPPED', null, { kind: d.kind, ok: ok, dirty: dirty });
        }
    }

    // Document-URL trap (cycle 3, I1): EndLoad forces
    // g_oDocumentUrls.documentUrl to a file:// URL, and the
    // document image loaders (GlobalLoaders / editorscommon)
    // build file:///.../media/<name> URLs from it and load
    // them DIRECTLY — bypassing LocalFileGetImageUrlCorrect —
    // so WebKit blocks every stored image. Trap the property:
    // whatever sdkjs assigns, the stored value is our
    // joffice:// base, and getImageUrl yields
    // joffice:///docbase/media/<name>, which the scheme
    // handler serves from the staged extraction.
    (function installDocUrlTrap() {
        setInterval(function () {
            var ac = window.AscCommon || (getEditor().ew && getEditor().ew.AscCommon);
            var urls = ac && ac.g_oDocumentUrls;
            if (!urls || urls.__joTrapped) return;
            try {
                Object.defineProperty(urls, 'documentUrl', {
                    configurable: true,
                    enumerable: true,
                    get: function () { return 'joffice:///docbase'; },
                    set: function (v) {
                        if (!urls.__joLogged) {
                            urls.__joLogged = true;
                            console.log('[jOffice] documentUrl trapped (sdkjs assigned: ' + v + ')');
                        }
                    }
                });
                urls.__joTrapped = true;
            } catch (e) { /* retry next tick */ }
        }, 25);
    })();

    (function installSaveGuards() {
        setInterval(function () {
            var ref = getEditor();
            var ed = ref && ref.editor;
            if (!ed) return;
            if (typeof ed.asc_Save === 'function' && !ed.asc_Save.__joWrapped) {
                var orig = ed.asc_Save;
                var wrapped = function () {
                    joSerializedSave(ed, orig, Array.prototype.slice.call(arguments));
                };
                wrapped.__joWrapped = true;
                try {
                    ed.asc_Save = wrapped;
                    console.log('[jOffice][save-ledger] asc_Save serialization wrap asserted');
                } catch (e) { /* prototype-only object; retry next tick */ }
            }
            if (!ed.__joLusiTrapped) {
                try {
                    var cur = ed.LastUserSavedIndex;
                    Object.defineProperty(ed, 'LastUserSavedIndex', {
                        configurable: true,
                        enumerable: true,
                        get: function () { return cur; },
                        set: function (v) {
                            if (joSave.cycle && v !== undefined) {
                                joLedger('LUSI_WRITE_IN_CYCLE', joSave.pendingSeq, {
                                    value: v,
                                    stack: (new Error().stack || '').split('\n').slice(1, 4).join(' | ')
                                });
                            }
                            cur = v;
                        }
                    });
                    ed.__joLusiTrapped = true;
                    console.log('[jOffice][save-ledger] LastUserSavedIndex trap asserted');
                } catch (e) { /* leave untrapped; the ledger notes it once */
                    ed.__joLusiTrapped = 'failed';
                }
            }
        }, 25);
        // SAVE_ORPHAN watchdog (round 2 §A): a cycle open for
        // over two minutes is a lost EndSave by definition.
        setInterval(function () {
            if (joSave.cycle && !joSave.cycle.orphanLogged
                && Date.now() - joSave.cycle.t > 120000) {
                joSave.cycle.orphanLogged = true;
                joLedger('SAVE_ORPHAN', joSave.pendingSeq, { ageMs: Date.now() - joSave.cycle.t });
            }
        }, 30000);
    })();

    // Entry points Rust calls (via evaluate_javascript on the
    // TOP frame) for the close prompt's Save choice and the
    // error presenter's Retry. They reach into the editor frame
    // and drive asc_Save — never LocalFileSave directly — so the
    // cell editor commits first (round 1 §3).
    window.__joRequestSave = function (mode) {
        var ref = getEditor();
        if (!ref.editor) return JSON.stringify({ started: false, error: 'no-editor' });
        var ew = ref.ew || window;
        if (ew.__joSave) ew.__joSave.started = false;
        try {
            if (mode === 'saveas') ref.editor.asc_Save(false, true);
            else ref.editor.asc_Save();
        } catch (e) {
            return JSON.stringify({ started: false, error: String(e) });
        }
        var started = ew.__joSave ? !!ew.__joSave.started : false;
        return JSON.stringify({ started: started });
    };
    // Export as PDF… / Print… (cycle 3): driven by the native
    // action bar and by the printRequest bridge. Both go
    // through the editor's own Download-As machinery
    // (asc_DownloadAs → asc_Save with isSaveAs) — never
    // asc_nativeGetFile directly — so the save ledger, the
    // serialization wrapper, and the EndSave contract all
    // apply unchanged. Print adds isPdfPrint, which the
    // LocalFileSave JSON options carry to Rust as the print
    // intent.
    window.__joExportPdf = function () {
        var ref = getEditor();
        if (!ref.editor || typeof ref.editor.asc_DownloadAs !== 'function') return false;
        ref.editor.asc_DownloadAs(joDownloadOptions(513, false));
        return true;
    };
    window.__joPrintExport = function () {
        var ref = getEditor();
        if (!ref.editor || typeof ref.editor.asc_DownloadAs !== 'function') return false;
        ref.editor.asc_DownloadAs(joDownloadOptions(513, true));
        return true;
    };
    // The Download-As options object sdkjs expects (pinned
    // AdvancedOptions.js): a real asc_CDownloadOptions, not a
    // bare literal — StartSave reads fileType / isPdfPrint /
    // nativeOptions off it.
    function joDownloadOptions(fileType, isPrint) {
        var A = window.Asc || (getEditor().ew && getEditor().ew.Asc);
        if (A && typeof A.asc_CDownloadOptions === 'function') {
            var o = new A.asc_CDownloadOptions(fileType, false);
            if (isPrint) o.isPdfPrint = true;
            return o;
        }
        return { fileType: fileType, isPdfPrint: !!isPrint, isNaturalDownload: false };
    }
    // Drive/diagnostic channel (cycle 4): a JSON snapshot of
    // the save/journal state, and an eval forwarder so the
    // shell's env-gated drive hook can run code in the EDITOR
    // frame (where the document lives), not just the top frame.
    window.__joState = function () {
        var ref = getEditor();
        var ed = ref.editor;
        var hist = null;
        try {
            var w = ref.ew || window;
            hist = w.AscCommon && w.AscCommon.History;
        } catch (e) { /* frame gone */ }
        return JSON.stringify({
            histIdx: hist ? hist.Index : null,
            savedIdx: hist ? hist.SavedIndex : null,
            userSavedIdx: hist ? hist.UserSavedIndex : null,
            lusi: ed ? ed.LastUserSavedIndex : null,
            canSave: ed ? ed.canSave : null,
            modified: ed && typeof ed.isDocumentModified === 'function'
                ? !!ed.isDocumentModified() : null,
            pendingSeq: joSave.pendingSeq,
            startSaveOpen: joSave.startSaveOpen,
            journal: {
                sid: joJournal.sid, enabled: joJournal.enabled,
                epoch: joJournal.epoch, seq: joJournal.seq,
                base: joJournal.base, openChanges: joJournal.openChanges,
                histLen: joJournal.histLen || 0
            },
            flushMode: joFlushMode()
        });
    };
    window.__joDriveEval = function (code) {
        try {
            // If THIS frame is the editor frame, run here.
            if (window.Asc && window.editor && window.AscCommon
                && window.AscCommon.History) {
                var r0 = (0, eval)(code);
                return JSON.stringify({ ok: true, value: r0 === undefined ? null : r0 });
            }
            var ref = getEditor();
            var w = ref.ew || window;
            var r = w.eval(code);
            return JSON.stringify({ ok: true, value: r === undefined ? null : r });
        } catch (e) {
            return JSON.stringify({ ok: false, error: String(e) });
        }
    };
    window.__joSaveStarted = function () {
        var ref = getEditor();
        var ew = ref.ew || window;
        if (ew.__joSave) {
            return JSON.stringify({
                started: !!ew.__joSave.started,
                seen: !!ew.__joSave.startSaveSeen,
                pendingSeq: ew.__joSave.pendingSeq
            });
        }
        return JSON.stringify({ started: false, seen: false, pendingSeq: null });
    };
    // Direct dirty query for the close path's MODIFIED_DISAGREE
    // bookkeeping (Rust compares it against its mirror). The
    // pushed/cellEditing values live in the EDITOR frame's
    // joSave — this function runs in the top frame, so reach
    // through ref.ew like __joRequestSave does.
    window.__joQueryModified = function () {
        var ref = getEditor();
        var direct = null;
        try {
            if (ref.editor && typeof ref.editor.isDocumentModified === 'function') {
                direct = !!ref.editor.isDocumentModified();
            }
        } catch (e) { /* leave null */ }
        var ew = ref.ew || window;
        var es = ew.__joSave || joSave;
        return JSON.stringify({
            direct: direct,
            pushed: es.lastPushedModified,
            cellEditing: es.cellEditing
        });
    };

    window.AscDesktopEditor = {
        // Host feature bag. desktopinit.js sets window.desktop = this
        // object and web-apps mutates .features (e.g. singlewindow).
        features: {},

        // The host command channel. web-apps' Desktop controller
        // treats AscDesktopEditor as `native` and calls
        // native.execCommand('webapps:features' | 'editor:config' |
        // 'title:button' | 'doc:onready' | ...) for shell-side
        // services (title-bar buttons, alt hints, feature flags).
        // WITHOUT this method the Desktop controller module throws
        // during load and Common.Controllers.Desktop never defines
        // (first-boot failure, cycle 1: "native.execCommand is not
        // a function" -> "Desktop.isActive is not a function").
        // This shell draws no title-bar buttons and has no shell
        // services to offer yet: accept every command, log it (the
        // log IS the feature-negotiation record), change nothing.
        execCommand: function (cmd, param) {
            console.log('[jOffice] execCommand: ' + cmd);
        },

        // Called by the Desktop controller on document ready; in
        // the Qt shell it answers asynchronously with the recents
        // list via on_native_message. This shell keeps no recents
        // store yet — fire-and-forget no-op.
        LocalFileRecents: function () {
            console.log('[jOffice] LocalFileRecents (no recents store)');
        },

        // -------------------------------------------------------
        // Host-surface completion (cycle 1). sdkjs references 79
        // AscDesktopEditor members; a missing one is a TypeError
        // wherever the call is unguarded (GetInstallPlugins killed
        // boot exactly that way). Policy: every referenced member
        // exists; capability probes report this shell's REAL state
        // (no crypto, signatures, media, or print pipeline yet);
        // value getters return the source-verified desktop
        // defaults; action methods log and no-op. Nothing here
        // pretends a feature works.
        // -------------------------------------------------------

        // Encryption mode flag — a NUMBER property in sdkjs
        // (compared against 0, assigned by sdkjs itself).
        CryptoMode: 0,

        // editorscommon UpdateSystemPlugins does
        // JSON.parse(GetInstallPlugins()) and touches [0] and [1],
        // each {url, pluginsData[]}. A fresh desktop install ships
        // an empty sdkjs-plugins dir (verified in the official
        // Euro-Office 9.3.8 deb): two entries, no plugins.
        GetInstallPlugins: function () {
            return JSON.stringify([
                { url: 'joffice:///sdkjs-plugins/', pluginsData: [] },
                { url: 'joffice:///sdkjs-plugins-user/', pluginsData: [] }
            ]);
        },

        // device_scale.js: guarded call, expects an array of
        // numbers (the plugin-framework copy of this list is
        // [1, 1.25, 1.5, ..., 5] in sdkjs common/plugins.js).
        GetSupportedScaleValues: function () {
            return [1, 1.25, 1.5, 1.75, 2, 2.25, 2.5, 2.75, 3, 3.5, 4, 4.5, 5];
        },

        // commonDefines.js: guarded, desktop default is 28.
        GetFontThumbnailHeight: function () { return 28; },

        // libfont/engine.js: guarded; empty = no desktop
        // dictionaries path (spellcheck dictionaries not wired).
        getDictionariesPath: function () { return ''; },

        getEngineVersion: function () { return '9.3.8'; },

        // Capability probes — honest negatives.
        IsSignaturesSupport: function () { return false; },
        IsProtectionSupport: function () { return false; },
        isBlockchainSupport: function () { return false; },
        IsSupportMedia: function () { return false; },
        // Printing exists (cycle 3): the probe gates the
        // web-apps print UI, so it answers true now.
        IsFilePrinting: function () { return true; },
        isSupportMacroses: function () { return false; },
        isSupportPlugins: function () { return false; },
        // Synchronous answers from local caches (marathon 2
        // cycle 2): the drop cache Rust pushes in, and the file
        // names behind our own dialog handles.
        IsImageFile: function (path) {
            var cache = window.__joDropCache || [];
            for (var i = 0; i < cache.length; i++) {
                if (cache[i].handle === path) return !!cache[i].isImage;
            }
            var info = joImages.byHandle[path];
            if (info) return /\.(png|jpe?g|gif|bmp|svg|webp|emf|wmf|tiff?)$/i.test(info.name);
            return false;
        },
        IsLocalFileExist: function () { return false; },

        // Image helpers (media/drawing paths).
        GetImageFormat: function () { return ''; },
        GetImageOriginalSize: function () { return { width: 0, height: 0 }; },
        // sdkjs post-processes every document-image URL through
        // here (Local/common.js getCorrectImageUrl). An image WE
        // staged for insertion comes back as
        // file:///<docname>/media/<localName> — a file:// URL the
        // page cannot load. Rewrite it to the shell-served
        // docmedia URL (the scheme handler holds the request
        // until the staged copy lands). Anything else passes
        // through unchanged. (Marathon 2 cycle 2.)
        LocalFileGetImageUrlCorrect: function (url) {
            // Inserted images (cycle 2): rewrite only names the
            // insert path staged. Document-STORED media (cycle
            // 3, I1): EndLoad forces documentUrl to
            // file:///<doc path>, so EVERY stored image arrives
            // here as file:///.../media/<name> — rewrite those
            // to the docmedia route too; Rust extracted the
            // document's media at open and serves it from the
            // same handler. file: only: a name the extraction
            // did not stage simply 404s there instead of being
            // blocked as a file:// subresource.
            var m = /\/media\/([^\/\?#]+)$/.exec(url || '');
            if (m && joImages.byName[m[1]]) return 'joffice:///docmedia/' + m[1];
            if (m && typeof url === 'string' && url.indexOf('file:') === 0) {
                return 'joffice:///docmedia/' + m[1];
            }
            return url;
        },

        // Change-tracking count for the Local save path. In
        // journal mode this is the folded journal length — the
        // changes that exist only in the session journal, not
        // in the bound file (round 1 §3: a recovered window
        // reports the folded list length here, which is what
        // makes Have_Changes call it dirty at Index -1).
        LocalFileGetOpenChangesCount: function () {
            return joJournalMode() ? joJournal.openChanges : 0;
        },

        // Logged no-ops: crypto, signatures, printing, media,
        // plugins, external conversion. A call is never an error;
        // the log line is the record that the path was reached.
        AddChanges: function () { console.log('[jOffice] stub AddChanges'); },
        CallInAllWindows: function () { console.log('[jOffice] stub CallInAllWindows'); },
        CallMediaPlayerCommand: function () { console.log('[jOffice] stub CallMediaPlayerCommand'); },
        CompareDocumentUrl: function () { console.log('[jOffice] stub CompareDocumentUrl'); },
        CryptoDownloadAs: function () { console.log('[jOffice] stub CryptoDownloadAs'); },
        GetDefaultCertificate: function () { console.log('[jOffice] stub GetDefaultCertificate'); return ''; },
        // Dropped files (marathon 2 cycle 2): Rust records GTK
        // drops and pushes window.__joDropCache = [{handle, name,
        // isImage}]; the handles are the same opaque ids the file
        // dialog returns, so every downstream consumer
        // (LocalFileGetImageUrl, LocalFileOpen) resolves them
        // through the bridge unchanged.
        GetDropFiles: function () {
            var cache = window.__joDropCache || [];
            return cache.map(function (e) { return e.handle; });
        },
        GetEncryptedHeader: function () { console.log('[jOffice] stub GetEncryptedHeader'); return ''; },
        LoadJS: function () { console.log('[jOffice] stub LoadJS'); },
        MediaStart: function () { console.log('[jOffice] stub MediaStart'); },
        MergeDocumentUrl: function () { console.log('[jOffice] stub MergeDocumentUrl'); },
        NativeViewerOpen: function () { console.log('[jOffice] stub NativeViewerOpen'); },
        OnSave: function () { console.log('[jOffice] stub OnSave'); },
        OpenFileCrypt: function () { console.log('[jOffice] stub OpenFileCrypt'); },
        PluginInstall: function () { console.log('[jOffice] stub PluginInstall'); },
        PluginUninstall: function () { console.log('[jOffice] stub PluginUninstall'); },
        PreloadCryptoImage: function () { console.log('[jOffice] stub PreloadCryptoImage'); },
        // Printing (cycle 3, step 4): sdkjs's _printDesktop
        // hands us the desktop options JSON — nativeOptions
        // carries the range the editor's own print dialog
        // collected. Rust owns the rest: for document editors
        // it kicks a print-intent PDF export back through
        // asc_Save (the range rides along via joSave.
        // printPages); for a ViewOnly PDF it prints the
        // original bytes directly.
        Print: function (json) {
            var pages = null;
            try {
                var o = JSON.parse(json || '{}');
                var no = o.nativeOptions || (o.advancedOptions && o.advancedOptions.nativeOptions);
                if (no && no.pages != null) pages = String(no.pages);
                else if (o.pages != null) pages = String(o.pages);
            } catch (e) { /* range stays null = all pages */ }
            joSave.printPages = pages;
            console.log('[jOffice] Print requested pages=' + pages);
            bridge('printRequest', { pages: pages }).catch(function (e) {
                console.error('[jOffice] printRequest failed: ' + (e && e.message ? e.message : e));
            });
        },
        Print_Start: function () { console.log('[jOffice] stub Print_Start'); },
        Print_Page: function () { console.log('[jOffice] stub Print_Page'); },
        Print_End: function () { console.log('[jOffice] stub Print_End'); },
        RemoveAllSignatures: function () { console.log('[jOffice] stub RemoveAllSignatures'); },
        RemoveFile: function () { console.log('[jOffice] stub RemoveFile'); },
        RemoveSignature: function () { console.log('[jOffice] stub RemoveSignature'); },
        ResaveFile: function () { console.log('[jOffice] stub ResaveFile'); },
        SaveQuestion: function () { console.log('[jOffice] stub SaveQuestion'); },
        SelectCertificate: function () { console.log('[jOffice] stub SelectCertificate'); return ''; },
        // Ledger the call's LENGTH only (protected cycle):
        // the argument of a DRM open carries the document
        // password in its XML — it must never reach a log.
        // Rust owns the password; this stays a no-op stub.
        SetAdvancedOptions: function (xml) {
            joLedger('SETADVANCEDOPTIONS', null, { len: (xml && xml.length) || 0 });
        },
        SetLocalRestrictions: function () { console.log('[jOffice] stub SetLocalRestrictions'); },
        Sign: function () { console.log('[jOffice] stub Sign'); },
        // The desktop spellcheck route (cycle 7): sdkjs hands
        // us the task as a JSON string (or the bare string
        // 'clear' for a restart) and waits for exactly one
        // asc_nativeOnSpellCheck reply. Forward to Rust's
        // spell service; the answer rides __joDeliverSpellReply.
        // A bridge failure settles the waiter with the local
        // fallback echo — a stalled document is the one
        // outcome this function must never produce.
        SpellCheck: function (arg) {
            try {
                var taskStr = (typeof arg === 'string') ? arg : JSON.stringify(arg);
                var seq = ++joSpell.seq;
                joSpell.sent++;
                if (taskStr === 'clear') {
                    // Restart semantics (stock): queued tasks
                    // are dropped; the single "clear" reply
                    // settles the editor's pending state.
                    var dropped = Object.keys(joSpell.waiters).length;
                    joSpell.waiters = {};
                    joLedger('SPELL_CLEAR', seq, { dropped: dropped });
                }
                var paraId = null;
                if (taskStr !== 'clear') {
                    try { paraId = JSON.parse(taskStr).ParagraphId; } catch (e) { /* cell tasks may lack it */ }
                }
                joSpell.waiters[seq] = { task: taskStr, paraId: paraId, at: Date.now() };
                if (joSpell.sent <= 5 || joSpell.sent % 100 === 0) {
                    joLedger('SPELL_TASK_SENT', seq, { n: joSpell.sent });
                }
                bridge('spellTask', { task: taskStr, spellSeq: seq }).catch(function (e) {
                    joLedger('SPELL_BRIDGE_FAIL', seq, { err: String(e) });
                    var w = joSpell.waiters[seq];
                    if (w) {
                        delete joSpell.waiters[seq];
                        joSpell.answered++;
                        var val = joSpellFallback(w.task);
                        if (typeof window.asc_nativeOnSpellCheck === 'function') {
                            try { window.asc_nativeOnSpellCheck(val); } catch (e2) { /* ignore */ }
                        }
                    }
                });
            } catch (e) {
                console.log('[jOffice] SpellCheck shim error: ' + e);
            }
        },
        ViewCertificate: function () { console.log('[jOffice] stub ViewCertificate'); },
        buildCryptedEnd: function () { console.log('[jOffice] stub buildCryptedEnd'); },
        buildCryptedStart: function () { console.log('[jOffice] stub buildCryptedStart'); },
        emulateCloudPrinting: function () { console.log('[jOffice] stub emulateCloudPrinting'); },
        localSaveToDrawingFormat: function () { console.log('[jOffice] stub localSaveToDrawingFormat'); },
        nativeClipboardWrite: function () { console.log('[jOffice] stub nativeClipboardWrite'); },
        onDocumentContentReady: function () { console.log('[jOffice] stub onDocumentContentReady'); },
        // The modified-state push channel (marathon 2 cycle 2;
        // Kagi round 2 §B/§F: this notification is AUTHORITATIVE —
        // the shell mirrors it in Rust and trusts the mirror
        // instead of polling JS at close time). sdkjs calls this
        // with History.Have_Changes(undefined, true): "has edits
        // beyond the saved index", NOT "needs saving" — the
        // format-policy part of dirtiness is the shell's own
        // business (Rust adds it). The Cell editor suppresses
        // this call while canSave is false (during a save) —
        // the save/dirty ledger exists to catch exactly the
        // edits that fall into that hole.
        onDocumentModifiedChanged: function (bModified) {
            var v = !!bModified;
            // Cycle 4, W2 finding: Have_Changes is measured
            // against the EDITOR's saved index, which any
            // completed save gesture advances — including an
            // export or print, which write no bound file. In
            // journal mode the shell's dirty truth is the
            // history position vs the last BOUND save (round 2
            // §6): items beyond boundHistLen mean the document
            // is dirty whatever the editor's saved index says.
            // (Undo-all returns histLen to boundHistLen, so a
            // fully-undone document stays clean — W4.)
            var overlay = false;
            if (!v && joJournalMode() && joJournal.enabled
                && (joJournal.histLen || 0) !== (joJournal.boundHistLen || 0)) {
                v = true;
                overlay = true;
            }
            joSave.lastPushedModified = v;
            joSave.overlayForced = overlay;
            joLedger('modified', null, { pushed: v, overlay: overlay });
            bridge('modifiedChanged', { value: v }).catch(function () { /* log-only channel */ });
            // Cycle 7 Dead probe feeder: edits while no spell
            // task has ever arrived are the liveness machine's
            // "typing produced no task" evidence. Throttled;
            // Rust latches the first one and ignores the rest.
            if (v && joSpell.sent === 0) {
                var nowMs = Date.now();
                if (nowMs - joSpellProbe.editedSentAt > 5000) {
                    joSpellProbe.editedSentAt = nowMs;
                    bridge('spellEvent', { kind: 'edited' }).catch(function () { /* log-only channel */ });
                }
            }
        },
        openExternalReference: function () { console.log('[jOffice] stub openExternalReference'); },
        sendSystemMessage: function () { console.log('[jOffice] stub sendSystemMessage'); },
        startExternalConvertation: function () { console.log('[jOffice] stub startExternalConvertation'); },

        IsLocalFile: function () { return true; },
        GetEditorId: function () { return 'joffice'; },
        CheckNeedWheel: function () { return true; },
        CheckUserId: function () { return 'local-user'; },
        isSupportBinaryFontsSprite: false,
        isSupportNetworkFunctionality: function () { return false; },
        LocalFileGetSourcePath: function () { return currentDocName; },
        // "Is the document bound to a saved local file in its
        // native format?" (marathon 2 cycle 2). History.Have_
        // Changes consults this for a fresh document: false makes
        // even an unedited document count as unsaved — which is
        // precisely the format-loss hook for new documents and
        // non-InPlace formats (Kagi round 1 §3). Rust seeds the
        // value at open (pendingFile/openFile replies); a bound
        // save or an adopted Save As sets it; an export leaves it.
        LocalFileGetSaved: function () { return joSave.boundSaved; },

        getFontsSprite: function (suffix) {
            suffix = suffix || '';
            return 'joffice:///sdkjs/common/Images/fonts_thumbnail' + suffix + '.png';
        },

        // sdkjs hands us the editor API object. KEEP IT. Do not discard
        // it and scan iframes — the editor may run in the top document.
        CreateEditorApi: function (api) {
            editorApi = api || null;
            // Cycle 7: the language-list filter and the
            // spellcheck toggle mirror must be in place before
            // DesktopAfterOpen can fire (the open path calls
            // EndLoad, which calls DesktopAfterOpen).
            joInstallSpellFilter(editorApi);
            joWrapSpellToggle(editorApi);
        },

        // Called by DesktopOfflineUpdateLocalName during load.
        // Without this, the load throws.
        SetDocumentName: function (name) {
            currentDocName = name || '';
        },

        // Used when data starts with binary_content://.
        // Cycle 4: returns the buffer joOpen parked, EXACTLY
        // once — EndLoad treats a null return as a critical
        // open error, and a second call must not hand the same
        // buffer to a different document.
        GetOpenedFile: function (_data) {
            var buf = joOpenedSlot.buf;
            joOpenedSlot.buf = null;
            if (buf) {
                console.log('[jOffice] GetOpenedFile -> buffer ' + buf.byteLength + ' bytes');
            }
            return buf;
        },

        // Called by CDocsCoApi.saveChanges for change-tracking:
        // the JOINED changes string, the deleteIndex, and the
        // change count (common/Local/common.js, pinned 9.3.8).
        //
        // CYCLE 4 — this function must NEVER throw (round 2 §B):
        // it is called from inside Send_Changes, BEFORE
        // Reset_SavedIndex and onUnSaveLock run; an escaping
        // exception leaves canSave false forever and every later
        // save silently no-ops. The whole body is try/caught,
        // and the saveChanges wrap (below) catches again around
        // the original sdkjs call.
        //
        // Behavior by flush mode (window.__joFlushMode):
        //   journal / m1 — log-only (FLUSH_SEEN). The journal
        //     wrap on CDocsCoApi.saveChanges already recorded
        //     the changes; stock desktop never writes the
        //     user's file from a flush, and neither do we.
        //   m2 — E0: snapshot the live document via
        //     asc_nativeGetFile3 (no x2t, no write), time it
        //     (S0), and search the bytes for the drive marker.
        //   bound / m0 / m3 — the cycle-3 delegation to a full
        //     LocalFileSave (m3 differs only in Rust: the
        //     flush's conversion lands on a working-copy path
        //     instead of the bound file).
        LocalFileSaveChanges: function (_changes, _deleteIndex, _count) {
            var mode = joFlushMode();
            var lusiBefore = null, lusiAfter = null;
            try {
                var ref0 = getEditor();
                if (ref0.editor) lusiBefore = ref0.editor.LastUserSavedIndex;
            } catch (e) { /* ledger input only */ }
            try {
                joLedger('FLUSH_SEEN', null, {
                    mode: mode,
                    deleteIndex: _deleteIndex,
                    count: _count,
                    chars: (_changes && _changes.length) || 0
                });
                if (mode === 'journal' || mode === 'm1') {
                    // Log-only. Nothing else is owed.
                } else if (mode === 'm2') {
                    joE0Snapshot('flush');
                } else {
                    // bound / m0 / m3: cycle-3 delegation. A
                    // flush arriving while ANOTHER save is in
                    // flight trails — that save is a different
                    // gesture, the editor stays dirty, and the
                    // changes re-flush afterwards.
                    if (joSave.pendingSeq !== null) {
                        joSave.trailingFlush = true;
                        joLedger('SAVE_TRAILING_FLUSH', joSave.pendingSeq, {});
                    } else {
                        joSave.nextOrigin = 'changes';
                        window.AscDesktopEditor.LocalFileSave.apply(
                            window.AscDesktopEditor,
                            [_changes, undefined, undefined, undefined, undefined]);
                    }
                }
            } catch (e) {
                console.error('[jOffice][save-ledger] FLUSH_THROW swallowed: '
                    + (e && e.message ? e.message : e));
            }
            try {
                var ref1 = getEditor();
                if (ref1.editor) lusiAfter = ref1.editor.LastUserSavedIndex;
            } catch (e) { /* ledger input only */ }
            if (lusiBefore !== null && lusiAfter !== null && lusiBefore !== lusiAfter) {
                joLedger('FLUSH_MOVED_USI', null, { from: lusiBefore, to: lusiAfter, mode: mode });
            }
        },

        // Called by sdkjs when the editor is ready to open a document.
        // NO-OP when there is no pending file: do NOT open an empty doc
        // (that loops: openDocument -> onEndLoadFile -> LocalStartOpen).
        // Guard against firing twice.
        LocalStartOpen: function () {
            if (startOpenDone) return;
            startOpenDone = true;
            // Stage breadcrumbs for the large-file diagnosis
            // (cycle 3, step 5): the stage variable + a 1 Hz
            // heartbeat, mirrored to the shell's stdout by the
            // console forwarder — the drive pairs each beat with
            // WebProcess CPU from /proc to classify busy vs
            // waiting per stage.
            joOpenStage.stage = 'convert_requested';
            joOpenStage.t0 = Date.now();
            if (!joOpenStage.beat) {
                joOpenStage.beat = setInterval(function () {
                    console.log('[jOffice][heartbeat] stage=' + joOpenStage.stage
                        + ' t=' + Date.now());
                    if (joOpenStage.stage === 'ready'
                        && Date.now() - joOpenStage.t0 > 600000) {
                        clearInterval(joOpenStage.beat);
                        joOpenStage.beat = null;
                    }
                }, 1000);
            }
            setTimeout(function () {
                bridge('pendingFile').then(function (pending) {
                    if (!pending || !pending.handle) {
                        // No file to open. Leave the editor on its start
                        // screen; do not force an empty document.
                        return;
                    }
                    // Rust seeds the save-policy state with the
                    // pending-file reply (cycle 2): boundSaved is
                    // the LocalFileGetSaved value for this open.
                    if (pending.boundSaved !== undefined) joSave.boundSaved = !!pending.boundSaved;
                    return joBridgeOpen('convertFile', { path: pending.handle, forOpen: true }, joOpenWellFormed).then(function (res) {
                        if (!res || (!res.data && !res.binUrl)) throw new Error('convertFile returned no data');
                        if (res.name) currentDocName = res.name;
                        // Cycle 4: ONE open path (joOpen) for
                        // base64 and session-URL transports.
                        joOpenWithBin(res, pending.name || res.name || '', false);
                    });
                }).catch(function (e) {
                    console.error('[jOffice] LocalStartOpen failed: ' + (e && e.message ? e.message : e));
                });
            }, 100);
        },

        OpenFilenameDialog: function (filterType, allowMultiple, callback) {
            joBridgeOpen('openFileDialog', { filterType: filterType || 'any', allowMultiple: !!allowMultiple },
                function (res) { return !!res && ('handle' in res); })
                .then(function (res) {
                    if (res && res.handle && callback) {
                        // Remember the file name behind the
                        // handle: LocalFileGetImageUrl needs the
                        // extension to name the staged copy, and
                        // IsImageFile answers from it.
                        if (res.name) joImages.byHandle[res.handle] = { name: res.name };
                        callback(res.handle);
                    }
                    // Cancel: no callback, no error (round 2
                    // restatement 5a) — the caller's flow simply
                    // does not proceed.
                })
                .catch(function (e) {
                    console.error('[jOffice] OpenFilenameDialog failed: ' + (e && e.message ? e.message : e));
                });
        },

        LocalFileOpen: function (path) {
            // path is an opaque handle from OpenFilenameDialog.
            // Rust resolves it, converts via x2t, returns Editor.bin.
            joBridgeOpen('openFile', { path: path || null }, joOpenWellFormed).then(function (res) {
                if (res && (res.data || res.binUrl)) {
                    if (res.boundSaved !== undefined) joSave.boundSaved = !!res.boundSaved;
                    // Cycle 4: the same joOpen path as startup.
                    joOpenWithBin(res, res.name || '', true);
                }
            }).catch(function (e) {
                console.error('[jOffice] LocalFileOpen failed: ' + (e && e.message ? e.message : e));
            });
        },

        LocalFileSave: function (param, password, docinfo, fileType, jsonOptions) {
            var isSaveAs = param && param.indexOf('saveas=true') !== -1;
            var origin = joSave.nextOrigin || 'user';
            joSave.nextOrigin = null;
            // A document save supersedes a debounced changes
            // flush (cycle 3): the flush's changes are inside
            // the bin this save is about to write.
            if (joSave.flushTimer !== null && origin !== 'changes') {
                clearTimeout(joSave.flushTimer);
                joSave.flushTimer = null;
                joLedger('SAVE_FLUSH_SUPERSEDED', null, { origin: origin });
            }
            // The gesture's own document save ends the gesture
            // window (cycle 3, S1′): flushes fold only while
            // the gesture is still between asc_Save and here.
            if (origin !== 'changes' && joSave.gesture) {
                joSave.gesture = false;
                if (joSave.gestureTimer !== null) {
                    clearTimeout(joSave.gestureTimer);
                    joSave.gestureTimer = null;
                }
            }
            var ref = getEditor();
            // Every LocalFileSave takes the next sequence number
            // and becomes the pending save owed exactly one
            // EndSave (round 2 §C). Assigned before any early
            // return so even a guard failure closes the save out.
            if (joSave.pendingSeq !== null) {
                // SAVE_REENTRY (cycle 3, round 2 §G): the
                // serialization layers above make this
                // unreachable; if it ever fires, the ledger is
                // the evidence — the save still proceeds (a host
                // call is never dropped).
                joLedger('SAVE_REENTRY', joSave.pendingSeq, { origin: origin });
            }
            var seq = ++joSave.seq;
            if (joSave.absorbPending) {
                joSave.absorbSeq = seq;
                joSave.absorbPending = false;
            }
            joSave.pendingSeq = seq;
            // The Download-As JSON options (cycle 3): the fifth
            // argument LocalFileSave always received and the
            // shim always ignored. It carries isPrint (a print
            // export) and, merged by asc_Save, the desktop
            // nativeOptions with the print range.
            var opts = {};
            try { opts = jsonOptions ? JSON.parse(jsonOptions) : {}; } catch (e) { opts = {}; }
            var printPages = null;
            if (opts.nativeOptions && opts.nativeOptions.pages != null) {
                printPages = String(opts.nativeOptions.pages);
            } else if (joSave.printPages != null) {
                printPages = String(joSave.printPages);
            }
            joSave.printPages = null;
            var unmodified = false;
            try {
                unmodified = ref.editor && typeof ref.editor.isDocumentModified === 'function'
                    ? !ref.editor.isDocumentModified() : false;
            } catch (e) { /* leave false */ }
            // Journal mode (cycle 4): sdkjs calls the document
            // unmodified once a FLUSH has sent the changes —
            // but the flush journaled them; the bound file does
            // NOT hold them. "Unmodified" for the shell's §6a
            // shortcuts (no-op Ctrl+S, byte-copy Save As) must
            // mean the FILE holds the document: any journaled
            // item beyond the last bound save defeats it.
            if (unmodified && joJournalMode() && joJournal.enabled) {
                var unsavedItems = Math.max(0,
                    (joJournal.histLen || 0) - (joJournal.boundHistLen || 0));
                if (unsavedItems > 0) {
                    unmodified = false;
                    joLedger('UNMOD_FORCED', seq, { unsavedItems: unsavedItems });
                }
            }
            // Protected windows (protected cycle): there is
            // no journal to flush to, but sdkjs still marks
            // flushed changes saved — isDocumentModified goes
            // false while the bound file holds none of the
            // edits. The §6a shortcuts must never fire here:
            // only a verified re-seal makes the file current.
            if (unmodified && joProtectedDoc) {
                unmodified = false;
                joLedger('UNMOD_FORCED', seq, { reason: 'protected' });
            }
            joLedger('localfilesave', seq, {
                saveAs: isSaveAs, fileType: fileType || 0, origin: origin,
                isPrint: !!opts.isPrint, unmodified: unmodified
            });
            var proceed = function () {
            joCheckDisagree('save');
            console.log('[jOffice][measure] SAVE_STAGE begin seq=' + seq + ' t=' + Date.now());
            if (!ref.editor) {
                console.error('[jOffice] LocalFileSave aborted: no editor');
                joEndSave(seq, 'failed');
                return;
            }
            if (joPdf.active) {
                // Cycle 5: a PDF window saves its COMPILED
                // annotation changes, never an Editor.bin —
                // asc_nativeGetFile3 is meaningless here. Rust
                // merges them into the bound file (route A1)
                // behind the annotation count gate.
                joPdfCaptureCompiled(function (cap) {
                    if (!cap) {
                        console.error('[jOffice] LocalFileSave (pdf) aborted: compiled-changes capture failed');
                        joEndSave(seq, 'failed');
                        return;
                    }
                    joLedger('PDF_CAPTURED', seq, {
                        bytes: cap.bytes.length,
                        head: joPdfHead(cap.bytes),
                        annotCount: cap.annotCount,
                        via: cap.via,
                        streamKind: cap.streamKind || 'unknown',
                        streamBase: cap.streamBase,
                        streamDelta: cap.streamDelta
                    });
                    joPdfSendSave(seq, {
                        isSaveAs: isSaveAs, fileType: fileType || 0,
                        origin: origin, unmodified: unmodified,
                        printIntent: !!opts.isPrint, printPages: printPages
                    }, cap);
                });
                return;
            }
            // INSTRUMENTATION: asc_nativeGetFile must exist or Save
            // hard-fails. Log its presence on first save attempt.
            if (typeof ref.editor.asc_nativeGetFile !== 'function') {
                console.error('[jOffice] asc_nativeGetFile MISSING — save cannot work');
                joEndSave(seq, 'failed');
                return;
            }
            try {
                var tGet = Date.now();
                // Cycle 4 save transport: snapshot via
                // asc_nativeGetFile3 (raw bytes, no base64) and
                // assemble the bin; the upload path carries
                // large bins to Rust in 8 MiB slices, small bins
                // keep the proven base64 bridge. The base64
                // asc_nativeGetFile path remains as the fallback
                // if getFile3 is unavailable or malformed.
                // The snapshot decode is ASYNC (sliced): the
                // synchronous atob + char loop over a ~292M-char
                // payload blocked the main thread past WebKit's
                // 10 s IPC watchdog at 109.5 MB and the web
                // process was aborted mid-save (cycle 4,
                // B3-109). Everything from here to the end of
                // proceed() runs in the snapshot continuation.
                joGetFile3SnapshotAsync('save', function (snap) {
                try {
                var binBytes = null;
                if (snap) {
                    binBytes = joAssembleBin(snap);
                } else {
                    var binData = ref.editor.asc_nativeGetFile();
                    if (!binData) {
                        console.error('[jOffice] LocalFileSave aborted: asc_nativeGetFile returned no data');
                        joEndSave(seq, 'failed');
                        return;
                    }
                    binBytes = (typeof binData === 'string')
                        ? _b64ToBytes(btoa(binData)) : new Uint8Array(binData);
                    joLedger('GETFILE3_FALLBACK', seq, { len: binBytes.length });
                }
                console.log('[jOffice][measure] SAVE_STAGE getfile seq=' + seq
                    + ' ms=' + (Date.now() - tGet) + ' bin_bytes=' + binBytes.length);
                // E0 discriminator (cycle 4, round 2 §A): is the
                // drive's marker inside the bin this save is
                // about to convert? Searched in the RAW bytes,
                // before any x2t involvement.
                if (window.__joE0Marker) {
                    var mHit = joMarkerInBytes(binBytes);
                    var mHist = null;
                    try {
                        if (window.AscCommon && window.AscCommon.History) {
                            mHist = window.AscCommon.History.Index;
                        }
                    } catch (e) { /* ledger input only */ }
                    joLedger('BIN_HAS_MARKER', seq, {
                        where: 'save', hit: mHit || 'none', histIdx: mHist,
                        len: binBytes.length
                    });
                }
                // bin_index for the EndSave assertion (cycle 3):
                // the history position these bytes represent.
                try {
                    if (window.AscCommon && window.AscCommon.History) {
                        joSave.lastBinIndex = window.AscCommon.History.Index;
                        joSave.lastBinSeq = seq;
                    }
                } catch (e) { /* assertion input only */ }
                // Rust owns the dialogs and the format policy: it
                // routes Save As / SaveAsOnly / AskOnce from here.
                // Dual-channel reply (cycle 3): deferred
                // replies (native dialogs) intermittently
                // arrive as EMPTY objects through the
                // postMessage return_value — a cancelled Save
                // As then looked like a bound success. Rust
                // therefore ALSO delivers every save
                // completion by evaluating
                // window.__joDeliverSaveReply(seq, res) in
                // this frame; whichever channel delivers a
                // WELL-FORMED reply (one carrying `ok`) wins.
                var settleSaveReply = null;
                var replyPromise = new Promise(function (resolve) {
                    settleSaveReply = resolve;
                });
                joSave.replyWaiters[seq] = function (res) {
                    if (settleSaveReply) {
                        var s = settleSaveReply;
                        settleSaveReply = null;
                        delete joSave.replyWaiters[seq];
                        s(res);
                    }
                };
                var absorbThis = !!joSave.absorbNext;
                joSave.absorbNext = false;
                var saveArgs = {
                    saveAs: isSaveAs, fileType: fileType || 0,
                    origin: origin, seq: seq,
                    unmodified: unmodified,
                    absorb: absorbThis,
                    histIndex: joSave.lastBinIndex,
                    printIntent: !!opts.isPrint,
                    printPages: printPages,
                    pdfa: !!(opts.pdfa || opts.isPDFA)
                };
                var sendSave = function () {
                    bridge('saveFile', saveArgs)
                        .then(function (res) {
                            if (res && typeof res.ok !== 'undefined') {
                                if (joSave.replyWaiters[seq]) {
                                    joSave.replyWaiters[seq](res);
                                }
                            } else {
                                console.log('[jOffice][save-ledger] saveFile postMessage reply seq=' + seq + ' garbled (' + JSON.stringify(res) + '); awaiting JS-channel delivery');
                            }
                        })
                        .catch(function (e) {
                            if (joSave.replyWaiters[seq]) {
                                joSave.replyWaiters[seq]({ ok: false, failed: true });
                            }
                        });
                };
                // Transport choice (round 2 §E): bins over
                // 4 MiB — or when the drive forces it for the
                // Bp parity test — go through the chunked
                // upload endpoint; the bridge call then carries
                // only the reference. Without a session (should
                // not happen for an open document) base64 is the
                // only channel.
                var useUpload = !window.__joForceBase64
                    && (binBytes.length > 4194304 || window.__joForceUpload)
                    && !!joJournal.sid;
                if (useUpload) {
                    joUpload('save', String(seq), binBytes, function (ok, sha) {
                        if (!ok) {
                            console.error('[jOffice] save upload failed seq=' + seq);
                            if (joSave.replyWaiters[seq]) {
                                joSave.replyWaiters[seq]({ ok: false, failed: true });
                            }
                            return;
                        }
                        joLedger('SAVE_UPLOAD', seq, { bytes: binBytes.length, sha256: sha });
                        saveArgs.uploadRef = seq;
                        sendSave();
                    });
                } else {
                    saveArgs.data = _bytesToB64(binBytes);
                    sendSave();
                }
                replyPromise
                    .then(function (res) {
                        console.log('[jOffice][measure] SAVE_STAGE bridge_done seq=' + seq + ' t=' + Date.now());
                        console.log('[jOffice][save-ledger] saveFile reply seq=' + seq + ' res=' + JSON.stringify(res));
                        if (res && res.failed) {
                            joEndSave(seq, 'failed');
                            return;
                        }
                        if (res && res.cancelled) {
                            // A cancelled dialog still owes the
                            // editor its one EndSave(1): StartSave
                            // blocked input, and only EndSave
                            // unblocks it (round 2 restatement 3).
                            joEndSave(seq, 'cancelled');
                            return;
                        }
                        if (res && res.export) {
                            // Export: the bound file was NOT
                            // saved; dirty state must be exactly
                            // what it was before (corrected S2).
                            joEndSave(seq, 'export_ok');
                            return;
                        }
                        if (res && res.printed !== undefined) {
                            // A print (cycle 3): an export-class
                            // completion — the document's dirty
                            // state is untouched by printing.
                            joEndSave(seq, 'export_ok');
                            return;
                        }
                        if (!res || res.ok !== true) {
                            // H6 (cycle 4): an empty or garbled
                            // reply is a FAILURE, never a bound
                            // success. Every real Rust reply
                            // carries an explicit ok field; the
                            // JS channel (__joDeliverSaveReply)
                            // had no shape check, so an empty
                            // object settled the save as bound_ok
                            // with nothing written — the cycle-3
                            // phantom-success shape, one channel
                            // over. Fail it; the shell's
                            // save_failed_outstanding guard then
                            // forces a real conversion next save.
                            console.error('[jOffice][save-ledger] saveFile reply seq=' + seq
                                + ' unrecognized shape; treating as failure');
                            joLedger('SAVE_REPLY_GARBLED', seq, {});
                            joEndSave(seq, 'failed');
                            return;
                        }
                        // Bound save (in place, or a Save As that
                        // adopted a native path): the document is
                        // now bound to a saved native file.
                        joSave.boundSaved = true;
                        joEndSave(seq, 'bound_ok');
                    })
                    .catch(function (e) {
                        console.error('[jOffice] LocalFileSave failed: ' + (e && e.message ? e.message : e));
                        joEndSave(seq, 'failed');
                    });
                } catch (e) {
                    console.error('[jOffice] LocalFileSave error: ' + (e && e.message ? e.message : e));
                    joEndSave(seq, 'failed');
                }
                });
            } catch (e) {
                console.error('[jOffice] LocalFileSave error: ' + (e && e.message ? e.message : e));
                joEndSave(seq, 'failed');
            }
            };
            proceed();
        },

        LocalFileCreate: function (type) {
            joSave.boundSaved = false;
            return bridge('createNew', { docType: type || 'word' });
        },

        // Insert-image naming (marathon 2 cycle 2; round 2
        // restatement 5a). SYNCHRONOUS: sdkjs uses the return
        // value immediately. For one of OUR dialog/drop handles
        // we choose the staged name (media/ins_<n>.<ext>),
        // remember it, and fire the copy to Rust in the
        // background. sdkjs then wraps the name as
        // file:///<doc>/media/<name>, and LocalFileGetImageUrl-
        // Correct (above) rewrites that to the docmedia URL the
        // scheme handler serves — holding the request until the
        // copy lands. Anything not ours passes through.
        LocalFileGetImageUrl: function (url) {
            if (!url) return url;
            if (url.indexOf('data:') === 0 || url.indexOf('blob:') === 0) return url;
            var info = joImages.byHandle[url];
            if (!info) {
                // A dropped file: Rust's drop cache also carries
                // names behind handles.
                var cache = window.__joDropCache || [];
                for (var i = 0; i < cache.length; i++) {
                    if (cache[i].handle === url) {
                        info = joImages.byHandle[url] = { name: cache[i].name };
                        break;
                    }
                }
            }
            if (!info) return url;
            if (info.localName) return info.localName;
            var ext = (info.name.split('.').pop() || 'png').toLowerCase();
            var localName = 'ins_' + (++joImages.counter) + '.' + ext;
            info.localName = localName;
            // K6: a new image binding exists only in this
            // editor's image table; the journal must fold the
            // document into a new seed at the insert's flush
            // or a crash loses the binding (see joJournalRecord).
            joJournal.imageStaged = true;
            joImages.byName[localName] = info;
            console.log('[jOffice] LocalFileGetImageUrl staged ' + localName + ' for ' + info.name);
            bridge('stageImage', { handle: url, name: localName }).catch(function (e) {
                console.error('[jOffice] stageImage failed: ' + (e && e.message ? e.message : e));
            });
            return localName;
        },

        DownloadFiles: function (urls, otherParams, callback) {
            if (!urls || !urls.length) {
                if (callback) callback({});
                return;
            }
            // Offline: no download. Return empty map.
            if (callback) callback({});
        },

        convertFile: function (filePath, targetFormat, callback) {
            joBridgeOpen('convertFile', { path: filePath, targetFormat: targetFormat || 0 }, joOpenWellFormed)
                .then(function (res) {
                    if (!res || !res.data) {
                        if (callback) callback(null);
                        return;
                    }
                    var bytes = _b64ToBytes(res.data);
                    var fileObj = {
                        _data: bytes,
                        _images: res.images || {},
                        get: function () { return this._data; },
                        getImages: function () { return this._images; },
                        close: function () { this._data = null; this._images = null; }
                    };
                    if (callback) callback(fileObj);
                })
                .catch(function (e) {
                    console.error('[jOffice] convertFile failed: ' + (e && e.message ? e.message : e));
                    if (callback) callback(null);
                });
        },

        // Compare / merge: decode base64 to raw bytes, pass BYTE length.
        // Passing base64 length / base64 string is wrong.
        CompareDocumentFile: function (file, oOptions) {
            joBridgeOpen('convertFile', { path: file }, joOpenWellFormed).then(function (res) {
                var ref = getEditor();
                if (ref.ew && ref.ew.onDocumentCompare && res && res.data) {
                    var bytes = _b64ToBytes(res.data);
                    ref.ew.onDocumentCompare('', bytes, bytes.length, res.images || {}, oOptions);
                }
            }).catch(function (e) {
                console.error('[jOffice] CompareDocumentFile failed: ' + (e && e.message ? e.message : e));
            });
        },

        MergeDocumentFile: function (file, oOptions) {
            joBridgeOpen('convertFile', { path: file }, joOpenWellFormed).then(function (res) {
                var ref = getEditor();
                if (ref.ew && ref.ew.onDocumentMerge && res && res.data) {
                    var bytes = _b64ToBytes(res.data);
                    ref.ew.onDocumentMerge('', bytes, bytes.length, res.images || {}, oOptions);
                }
            }).catch(function (e) {
                console.error('[jOffice] MergeDocumentFile failed: ' + (e && e.message ? e.message : e));
            });
        }
    };

    // -----------------------------------------------------------
    // Journal wrap (marathon 2 cycle 4, mode J; KAO round 2 §C
    // is the design authority, read against the pinned 9.3.8
    // sources).
    //
    // CDocsCoApi.prototype.saveChanges receives the RAW change
    // array before the Local override joins it into a string
    // for AscDesktopEditor.LocalFileSaveChanges. The wrap:
    //   * asserts the wire format of every element
    //     (<len>;<base64>) — a mismatch means the format
    //     changed under us, so the record is NOT journaled and
    //     an epoch checkpoint (a full getFile3 snapshot) takes
    //     its place;
    //   * assigns epoch + seq synchronously, in call order;
    //   * takes epoch checkpoints on the four correctness
    //     triggers (format / history_cleared / undo_then_type /
    //     below_base) — the checkpoint snapshot is taken
    //     synchronously HERE, while the document equals every
    //     point being sent, and the triggering record is
    //     absorbed into the new seed, never journaled;
    //   * NEVER throws, and never lets the original throw
    //     either: Send_Changes calls saveChanges BEFORE
    //     Reset_SavedIndex and onUnSaveLock, so an escaping
    //     exception wedges canSave false and silently kills
    //     every later save (round 2 §B; test T1).
    // -----------------------------------------------------------
    var JO_WIRE_RE = /^\d+;[A-Za-z0-9+\/=]*$/;

    // Chunked upload to the session endpoint (round 2 §E):
    // 8 MiB slices via XHR POST (Uint8Array.slice copies —
    // NEVER a Blob body; WebKitGTK's get_http_body SIGSEGVs on
    // Blob bodies), then a commit POST with the slice count
    // and exact length. Rust assembles, verifies, and answers
    // with the SHA-256. done(ok, sha256).
    function joUpload(kind, key, bytes, done) {
        var SLICE = 8 * 1024 * 1024;
        // Zero-length upload: commit directly with of=0. (An
        // XHR with a 0-length Uint8Array body over the custom
        // scheme never reaches the handler in WebKitGTK —
        // U0 caught it.)
        if (bytes.length === 0) {
            var cxhr = new XMLHttpRequest();
            var cbase = 'joffice:///session/' + joJournal.sid + '/upload/' + kind + '/' + key;
            cxhr.open('POST', cbase + '/commit', true);
            try {
                cxhr.setRequestHeader('X-JOffice-Secret', window.__joSecret || '');
                cxhr.setRequestHeader('Content-Type', 'application/json');
            } catch (e) { /* as below */ }
            cxhr.onload = function () {
                if (cxhr.status !== 200) { done(false, null); return; }
                var sha = null;
                try { sha = JSON.parse(cxhr.responseText).sha256; } catch (e) { /* evidence only */ }
                done(true, sha);
            };
            cxhr.onerror = function () { done(false, null); };
            cxhr.send(JSON.stringify({ of: 0, len: 0 }));
            return;
        }
        var n = Math.max(1, Math.ceil(bytes.length / SLICE));
        var base = 'joffice:///session/' + joJournal.sid + '/upload/' + kind + '/' + key;
        var i = 0;
        function postSlice() {
            if (i >= n) { commit(); return; }
            var part = bytes.slice(i * SLICE, Math.min(bytes.length, (i + 1) * SLICE));
            var xhr = new XMLHttpRequest();
            xhr.open('POST', base + '/' + i, true);
            try { xhr.setRequestHeader('X-JOffice-Secret', window.__joSecret || ''); } catch (e) { /* header is defense in depth */ }
            xhr.onload = function () {
                if (xhr.status !== 200) { done(false, null); return; }
                i++;
                postSlice();
            };
            xhr.onerror = function () { done(false, null); };
            xhr.send(part);
        }
        function commit() {
            var xhr = new XMLHttpRequest();
            xhr.open('POST', base + '/commit', true);
            try {
                xhr.setRequestHeader('X-JOffice-Secret', window.__joSecret || '');
                xhr.setRequestHeader('Content-Type', 'application/json');
            } catch (e) { /* as above */ }
            xhr.onload = function () {
                if (xhr.status !== 200) { done(false, null); return; }
                var sha = null;
                try { sha = JSON.parse(xhr.responseText).sha256; } catch (e) { /* sha is evidence, not control flow */ }
                done(true, sha);
            };
            xhr.onerror = function () { done(false, null); };
            xhr.send(JSON.stringify({ of: n, len: bytes.length }));
        }
        postSlice();
    }
    window.__joUpload = joUpload;

    function joJournalSend(record) {
        // Records under 1 MB ride the bridge; larger ones go
        // through the session upload endpoint first and the
        // bridge call references the uploaded payload (round 1
        // §2c — the bridge's reply channel is not the limit
        // here, but a multi-MB postMessage per flush is a stall
        // risk on the editor's main thread).
        var payload = JSON.stringify(record.changes);
        if (payload.length < 1000000) {
            bridge('journalAppend', {
                sid: joJournal.sid, epoch: record.epoch, seq: record.seq,
                deleteIndex: record.deleteIndex, changes: record.changes,
                excel: record.excel || null
            }).catch(function (e) {
                joLedger('JOURNAL_APPEND_FAIL', null, {
                    seq: record.seq, err: String(e && e.message || e)
                });
            });
            return;
        }
        joUpload('journal', String(record.seq),
            new TextEncoder().encode(payload),
            function (ok) {
                if (!ok) {
                    joLedger('JOURNAL_UPLOAD_FAIL', null, { seq: record.seq });
                    return;
                }
                bridge('journalAppend', {
                    sid: joJournal.sid, epoch: record.epoch, seq: record.seq,
                    deleteIndex: record.deleteIndex, uploadRef: record.seq,
                    excel: record.excel || null
                }).catch(function (e) {
                    joLedger('JOURNAL_APPEND_FAIL', null, {
                        seq: record.seq, err: String(e && e.message || e)
                    });
                });
            });
    }

    function joJournalCheckpoint(trigger, histLenAfter) {
        if (joPdf.active) {
            // Cycle 5: never for PDF windows (see the journal
            // branch in joJournalRecord). Logged, not silent.
            joLedger('PDF_CHECKPOINT_REFUSED', null, { trigger: trigger });
            return;
        }
        // The CAPTURE is synchronous and exact (asc_native-
        // GetFile3 runs inside the saveChanges call, so the
        // serialized state is the state the triggering record
        // describes). The base64 decode and the commit are
        // async; records arriving while either is in flight
        // queue in JS and flush into the NEW epoch when the
        // commit lands — Rust commits seed-<epoch>.bin by
        // rename BEFORE any record of that epoch is appended
        // (round 2 §C). The decode went async in cycle 4 after
        // the synchronous atob + char loop tripped WebKit's
        // 10 s IPC watchdog at 109.5 MB (B3-109/S0-109).
        if (joJournal.checkpointBusy) {
            joLedger('JOURNAL_CHECKPOINT_BUSY', null, { trigger: trigger });
            return;
        }
        joJournal.checkpointBusy = true;
        joGetFile3SnapshotAsync('checkpoint:' + trigger, function (snap) {
        if (!snap) {
            joJournal.checkpointBusy = false;
            joLedger('JOURNAL_CHECKPOINT_FAIL', null, { trigger: trigger, why: 'snapshot' });
            // Records queued during the decode belong to the
            // OLD epoch (no new seed exists); send them there,
            // exactly as the commit-failure branch below does.
            var stuck = joJournal.pendingRecords || [];
            joJournal.pendingRecords = [];
            for (var s = 0; s < stuck.length; s++) joJournalSend(stuck[s]);
            return;
        }
        var bin = joAssembleBin(snap);
        var newEpoch = joJournal.epoch + 1;
        var finish = function (ok) {
            joJournal.checkpointBusy = false;
            if (ok) {
                joJournal.epoch = newEpoch;
                joJournal.base = histLenAfter;
                joJournal.seq = 0;
                joJournal.openChanges = joJournal.unsavedFloor || 0;
                joLedger('JOURNAL_CHECKPOINT', null, {
                    trigger: trigger, epoch: newEpoch, base: histLenAfter, binBytes: bin.length
                });
                var queued = joJournal.pendingRecords || [];
                joJournal.pendingRecords = [];
                for (var i = 0; i < queued.length; i++) joJournalSend(queued[i]);
            } else {
                joLedger('JOURNAL_CHECKPOINT_FAIL', null, { trigger: trigger, why: 'commit' });
                // Queued records stay in the OLD epoch: their
                // deleteIndex values are still valid there, and
                // the Rust fold's reset/BeyondEnd rules keep a
                // later recovery honest (prefix + banner).
                var queued2 = joJournal.pendingRecords || [];
                joJournal.pendingRecords = [];
                for (var j = 0; j < queued2.length; j++) joJournalSend(queued2[j]);
            }
        };
        var args = {
            secret: window.__joSecret || '',
            sid: joJournal.sid, epoch: newEpoch, trigger: trigger,
            base: histLenAfter
        };
        if (bin.length <= 4194304) {
            args.data = _bytesToB64(bin);
            bridge('journalCheckpoint', args).then(function (res) {
                finish(!!(res && res.ok));
            }).catch(function () { finish(false); });
        } else {
            joUpload('ckpt', String(newEpoch), bin, function (ok) {
                if (!ok) { finish(false); return; }
                args.uploadRef = newEpoch;
                bridge('journalCheckpoint', args).then(function (res) {
                    finish(!!(res && res.ok));
                }).catch(function () { finish(false); });
            });
        }
        });
    }

    function joJournalRecord(arrayChanges, deleteIndex, excel) {
        if (!joJournalMode() || !joJournal.enabled || !joJournal.sid) return;
        if (joJournal.replaying) return;
        var changes = arrayChanges || [];
        if (joPdf.active) {
            // Cycle 5 (round 2 worst-case session model): PDF
            // windows append raw records ONLY. No trigger
            // evaluation, no checkpoints — the fold math and
            // the getFile3 snapshot are meaningless for a PDF,
            // and a misread deleteIndex on replay would
            // resurrect, duplicate or drop an annotation that
            // a later incremental save makes permanent.
            var pdfHistLen = (deleteIndex === null || deleteIndex === undefined)
                ? changes.length : deleteIndex + changes.length;
            joJournal.seq += 1;
            joJournal.histLen = pdfHistLen;
            joJournal.openChanges = Math.max(0, pdfHistLen - joJournal.base,
                joJournal.unsavedFloor || 0);
            if (!joPdf.degradedLogged) {
                joPdf.degradedLogged = true;
                joLedger('PDF_JOURNAL_DEGRADED', null, {
                    reason: 'append-only records, no checkpoints'
                });
            }
            joJournalSend({
                epoch: joJournal.epoch, seq: joJournal.seq,
                deleteIndex: (deleteIndex === undefined ? null : deleteIndex),
                changes: changes.slice(), excel: excel || null
            });
            return;
        }
        // One-time sample of the real wire form (E0b evidence).
        if (!joJournal.sampleLogged) {
            joJournal.sampleLogged = true;
            joLedger('JOURNAL_SAMPLE', null, {
                first: changes.length ? String(changes[0]).slice(0, 48) : '(empty)',
                n: changes.length, deleteIndex: deleteIndex
            });
        }
        var formatOk = true;
        for (var i = 0; i < changes.length; i++) {
            if (typeof changes[i] !== 'string' || !JO_WIRE_RE.test(changes[i])) {
                formatOk = false;
                break;
            }
        }
        var histLenAfter = (deleteIndex === null || deleteIndex === undefined)
            ? changes.length : deleteIndex + changes.length;
        var trigger = null;
        if (!formatOk) trigger = 'format';
        else if ((deleteIndex === null || deleteIndex === undefined) && joJournal.seq > 0) {
            trigger = 'history_cleared';
        } else if (joJournal.anomaly) {
            trigger = 'undo_then_type';
            joJournal.anomaly = null;
        } else if (deleteIndex !== null && deleteIndex !== undefined
            && deleteIndex < joJournal.base) {
            trigger = 'below_base';
        }
        // Cycle 4 K6: an image inserted since the last
        // checkpoint binds its staged name only in the live
        // editor's image table — the journal's change records
        // carry local-user ids, not names, so a replay from
        // the pre-insert seed loses the binding and the
        // recovered document serializes a nameless image
        // (x2t then writes a dangling media reference; the
        // SAVE_MEDIA_MISSING guard refuses that save). Fold
        // the document into a new seed at the insert's own
        // flush: the snapshot carries the name.
        if (!trigger && joJournal.imageStaged) {
            trigger = 'image_staged';
        }
        if (trigger) {
            joJournal.imageStaged = false;
            joJournal.histLen = histLenAfter;
            joJournalCheckpoint(trigger, histLenAfter);
            return;
        }
        joJournal.seq += 1;
        joJournal.histLen = histLenAfter;
        joJournal.openChanges = Math.max(0, histLenAfter - joJournal.base,
            joJournal.unsavedFloor || 0);
        var record = {
            epoch: joJournal.epoch, seq: joJournal.seq,
            deleteIndex: (deleteIndex === undefined ? null : deleteIndex),
            changes: changes.slice(), excel: excel || null
        };
        if (joJournal.checkpointBusy) {
            (joJournal.pendingRecords = joJournal.pendingRecords || []).push(record);
            joLedger('JOURNAL_QUEUED', null, { seq: record.seq, epoch: record.epoch });
            return;
        }
        joJournalSend(record);
    }

    (function installJournalWrap() {
        setInterval(function () {
            var A = window.AscCommon;
            if (!A) return;
            if (A.CDocsCoApi && A.CDocsCoApi.prototype
                && typeof A.CDocsCoApi.prototype.saveChanges === 'function'
                && !A.CDocsCoApi.prototype.saveChanges.__joWrapped) {
                var orig = A.CDocsCoApi.prototype.saveChanges;
                var wrapped = function (arrayChanges, deleteIndex, excelAdditionalInfo) {
                    try {
                        joJournalRecord(arrayChanges, deleteIndex, excelAdditionalInfo);
                    } catch (e) {
                        joLedger('JOURNAL_WRAP_THROW', null, { err: String(e) });
                    }
                    try {
                        return orig.apply(this, arguments);
                    } catch (e) {
                        // Swallowed on purpose (round 2 §B): the
                        // caller is Send_Changes, and it still
                        // owes the editor Reset_SavedIndex +
                        // onUnSaveLock. A host-side failure must
                        // not wedge canSave.
                        joLedger('SAVECHANGES_THROW', null, { err: String(e) });
                    }
                };
                wrapped.__joWrapped = true;
                A.CDocsCoApi.prototype.saveChanges = wrapped;
                console.log('[jOffice][save-ledger] saveChanges journal wrap asserted');
            }
            // Create_NewPoint anomaly watch (round 2 §C): a new
            // point created BELOW the saved index is the
            // undo-then-type shape. The pinned 9.3.8 editors
            // already rescue SavedIndex inside Create_NewPoint;
            // this wrap is the never-cut safety net for every
            // other path that can move it.
            if (A.CHistory && A.CHistory.prototype
                && typeof A.CHistory.prototype.Create_NewPoint === 'function'
                && !A.CHistory.prototype.Create_NewPoint.__joWrapped) {
                var origCnp = A.CHistory.prototype.Create_NewPoint;
                var wrappedCnp = function () {
                    try {
                        if (this.SavedIndex !== null && this.SavedIndex !== undefined
                            && this.Index < this.SavedIndex) {
                            joJournal.anomaly = 'undo_then_type';
                            joLedger('CREATE_POINT_BELOW_SAVED', null, {
                                index: this.Index, savedIndex: this.SavedIndex
                            });
                        }
                    } catch (e) { /* watch only */ }
                    return origCnp.apply(this, arguments);
                };
                wrappedCnp.__joWrapped = true;
                A.CHistory.prototype.Create_NewPoint = wrappedCnp;
                console.log('[jOffice][save-ledger] Create_NewPoint anomaly wrap asserted');
            }
        }, 25);
    })();

    // -----------------------------------------------------------
    // Font-load survivability + ledger (marathon 2 cycle 1;
    // Kagi round 2 §6/§7 is the design authority).
    //
    // The stock CFontFileLoader.LoadFontAsync (sdkjs common/Local/
    // common.js) hangs document loading forever when a font fetch
    // fails: on a non-200 response it sets Status = 1 and returns
    // WITHOUT calling the callback, and it has no onerror handler
    // at all (marathon 1's 53% freeze). This wrap replaces the
    // prototype method the moment it exists — before the first
    // font request — with a faithful copy of the original body
    // plus the survival machinery:
    //   * Status stays within {-1, 2, 0, 1} (sdkjs compares
    //     against these exact values; a new one could rehang).
    //   * The callback fires AT MOST once, on success AND on
    //     failure — failure now ends the wait instead of freezing.
    //   * Shim-owned bookkeeping lives in this._jo =
    //     {phase, t0, bytes, reason}; Status stays clean.
    //   * Missing vs slow is decided by RUST, not guessed here:
    //     a missing font gets an immediate labeled answer from
    //     the scheme handler (fallback bytes + the
    //     X-JOffice-Font-Substitute header, or an error status
    //     with X-JOffice-Font: missing). At 10 s still loading we
    //     log FONT_SLOW and keep waiting; we fail the load only
    //     after ~90 s with zero download progress (a dead read).
    //   * Ledger: JS start/end per font Id, logged to the console
    //     (the shell mirrors console to stdout). Rust logs its
    //     side per request; the drive compares the two — a Rust
    //     request with no JS entry means a code path bypassed
    //     this wrap (Kagi §7's riskiest assumption, tested).
    // -----------------------------------------------------------
    window.__joFontSubstitutes = window.__joFontSubstitutes || [];
    (function installFontLoaderWrap() {
        // The wrap must be RE-ASSERTED, not installed once: the
        // CFontFileLoader class appears before its desktop
        // LoadFontAsync does, and Local/common.js ASSIGNS the
        // real implementation onto the prototype when it loads —
        // overwriting any wrap placed earlier (drive 1 of this
        // cycle: wrap logged as installed, then all 23 font
        // fetches ran unwrapped — the §7 ledger caught it).
        // So: poll for the lifetime of the page; whenever the
        // current prototype method is not ours, replace it.
        var asserted = 0;
        setInterval(function () {
            var loader = window.AscFonts && window.AscFonts.CFontFileLoader;
            if (loader && loader.prototype
                && typeof loader.prototype.LoadFontAsync === 'function'
                && loader.prototype.LoadFontAsync !== joLoadFontAsync) {
                loader.prototype.LoadFontAsync = joLoadFontAsync;
                asserted++;
                console.log('[jOffice][font-ledger] LoadFontAsync wrap asserted (' + asserted + ')');
            }
        }, 25);

        function joLoadFontAsync(basePath, callback) {
            this.callback = callback;
            if (-1 !== this.Status)
                return true;

            this.Status = 2;
            var fontFile = this;
            var jo = this._jo = {
                phase: 'loading',
                t0: (window.performance && performance.now) ? performance.now() : Date.now(),
                bytes: 0,
                reason: null
            };
            var fired = false;
            function fireCallbackOnce() {
                if (fired) return;
                fired = true;
                if (null != fontFile.callback)
                    fontFile.callback();
                if (fontFile["externalCallback"])
                    fontFile["externalCallback"]();
            }
            function ledgerEnd(reason) {
                jo.phase = 'done';
                jo.reason = reason;
                var now = (window.performance && performance.now) ? performance.now() : Date.now();
                console.log('[jOffice][font-ledger] JS end id=' + fontFile.Id
                    + ' status=' + fontFile.Status
                    + ' ms=' + Math.round(now - jo.t0)
                    + ' bytes=' + jo.bytes
                    + ' reason=' + reason);
            }
            console.log('[jOffice][font-ledger] JS start id=' + fontFile.Id);

            var xhr = new XMLHttpRequest();
            xhr.open('GET', "ascdesktop://fonts/" + fontFile.Id, true);
            xhr.responseType = 'arraybuffer';

            if (xhr.overrideMimeType)
                xhr.overrideMimeType('text/plain; charset=x-user-defined');
            else
                xhr.setRequestHeader('Accept-Charset', 'x-user-defined');

            var lastProgressAt = Date.now();
            xhr.onprogress = function (e) {
                lastProgressAt = Date.now();
                if (e && e.loaded) jo.bytes = e.loaded;
            };

            // FONT_SLOW at 10 s: log, mark, keep waiting (Kagi §6).
            var slowTimer = setTimeout(function () {
                if (fontFile.Status === 2) {
                    jo.phase = 'slow';
                    console.log('[jOffice][font-ledger] FONT_SLOW id=' + fontFile.Id + ' (still loading at 10s)');
                }
            }, 10000);
            // Hard fail at ~90 s ONLY when the read shows no
            // progress at all — a live stream is never killed.
            var deadTimer = setTimeout(function () {
                if (fontFile.Status === 2 && Date.now() - lastProgressAt >= 89000) {
                    console.error('[jOffice][font-ledger] FONT_DEAD id=' + fontFile.Id + ' (no progress in 90s)');
                    try { xhr.abort(); } catch (e) { /* already gone */ }
                    fontFile.Status = 1;
                    ledgerEnd('dead-no-progress-90s');
                    fireCallbackOnce();
                }
            }, 90000);

            xhr.onload = function () {
                clearTimeout(slowTimer);
                clearTimeout(deadTimer);
                if (xhr.status !== 200) {
                    // THE FIX: the original set Status = 1 here and
                    // never called the callback — the document load
                    // waited forever. Fire it: the font is absent,
                    // the document proceeds without it.
                    fontFile.Status = 1;
                    ledgerEnd('http-' + xhr.status);
                    fireCallbackOnce();
                    return;
                }

                var substitute = null;
                try { substitute = xhr.getResponseHeader('X-JOffice-Font-Substitute'); } catch (e) { /* headers unavailable */ }

                fontFile.Status = 0;

                let fontStreams = AscFonts.g_fonts_streams;
                let streamIndex = fontStreams.length;
                if (xhr.response) {
                    let data = new Uint8Array(xhr.response);
                    jo.bytes = data.length;
                    fontStreams[streamIndex] = new AscFonts.FontStream(data, data.length);
                }
                else {
                    fontStreams[streamIndex] = AscFonts.CreateFontData3(xhr.responseText);
                }

                fontFile.SetStreamIndex(streamIndex);

                if (substitute) {
                    window.__joFontSubstitutes.push(substitute);
                    ledgerEnd('substituted:' + substitute);
                } else {
                    ledgerEnd('ok');
                }
                fireCallbackOnce();
            };

            xhr.onerror = function () {
                clearTimeout(slowTimer);
                clearTimeout(deadTimer);
                if (fontFile.Status !== 2) return;
                // The original has NO onerror handler: a scheme
                // error also froze the load. Same fix as non-200.
                fontFile.Status = 1;
                ledgerEnd('network-error');
                fireCallbackOnce();
            };

            xhr.send(null);
            return true;
        }
    })();

    // -----------------------------------------------------------
    // Page-jank probe (marathon 2 cycle 2 measurement, round 1
    // §5): requestAnimationFrame gaps in this frame for the
    // first three minutes. A gap is time the page could not
    // paint — the page-side half of the jank picture (the shell
    // logs its own GLib-timer stalls under JOFFICE_JANK).
    // -----------------------------------------------------------
    (function installRafProbe() {
        var t0 = performance.now();
        var last = t0;
        function loop(now) {
            var gap = now - last;
            last = now;
            if (gap > 250 && now - t0 > 1500) {
                console.log('[jOffice][measure] RAF_GAP ms=' + Math.round(gap) + ' t=' + Date.now());
            }
            if (now - t0 < 180000) requestAnimationFrame(loop);
        }
        requestAnimationFrame(loop);
    })();

    function _b64ToBytes(b64) {
        var raw = atob(b64);
        var bytes = new Uint8Array(raw.length);
        for (var i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
        return bytes;
    }

    // Sliced base64 decode for very large payloads: the naive
    // form above (one atob over the whole string + a per-char
    // loop) blocked the page main thread for minutes at
    // 109.5 MB and tripped WebKit's crashAfter10Seconds IPC
    // watchdog (cycle 4, B3-109/S0-109). Decoding in 4 MiB
    // slices inside setTimeout keeps the main thread
    // servicing IPC between slices. Slice boundaries are
    // multiples of 4 chars, so no base64 quartet is ever
    // split (padding can only appear in the final slice).
    // done(Uint8Array) on success, done(null) on a decode
    // error. (The pattern is the Bp harness's, promoted into
    // the product after it proved itself at 219 MB.)
    function _b64ToBytesSliced(b64, done) {
        var est = Math.floor(b64.length / 4) * 3;
        var out = new Uint8Array(est);
        var off = 0, pos = 0;
        var CH = 4194304;
        (function step() {
            var end = Math.min(b64.length, pos + CH);
            var bin;
            try {
                bin = atob(b64.substring(pos, end));
            } catch (e) {
                done(null);
                return;
            }
            for (var i = 0; i < bin.length; i++) out[off + i] = bin.charCodeAt(i);
            off += bin.length;
            pos = end;
            if (pos < b64.length) setTimeout(step, 0);
            else done(out.subarray(0, off));
        })();
    }

    function _bytesToB64(bytes) {
        var binary = '';
        for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        return btoa(binary);
    }
})();
