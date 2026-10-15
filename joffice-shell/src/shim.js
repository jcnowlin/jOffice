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
        return handlers.joffice.postMessage({ cmd: cmd, args: args || {} });
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

    window.AscDesktopEditor = {
        IsLocalFile: function () { return true; },
        GetEditorId: function () { return 'joffice'; },
        CheckNeedWheel: function () { return true; },
        CheckUserId: function () { return 'local-user'; },
        isSupportBinaryFontsSprite: false,
        isSupportNetworkFunctionality: function () { return false; },
        LocalFileGetSourcePath: function () { return currentDocName; },
        LocalFileGetSaved: function () { return false; },

        getFontsSprite: function (suffix) {
            suffix = suffix || '';
            return 'joffice:///sdkjs/common/Images/fonts_thumbnail' + suffix + '.png';
        },

        // sdkjs hands us the editor API object. KEEP IT. Do not discard
        // it and scan iframes — the editor may run in the top document.
        CreateEditorApi: function (api) {
            editorApi = api || null;
        },

        // Called by DesktopOfflineUpdateLocalName during load.
        // Without this, the load throws.
        SetDocumentName: function (name) {
            currentDocName = name || '';
        },

        // Used when data starts with binary_content://.
        GetOpenedFile: function (_data) {
            // No pre-opened file data in this shell; return null so
            // sdkjs falls through to the normal LocalStartOpen path.
            return null;
        },

        // Called by CDocsCoApi.saveChanges for change-tracking.
        // Without it, save-on-close / unsaved-changes tracking is dead.
        LocalFileSaveChanges: function (_param, _password, _docinfo, _fileType, _jsonOptions) {
            // Treat as a normal save: delegate to LocalFileSave.
            return window.AscDesktopEditor.LocalFileSave(_param, _password, _docinfo, _fileType, _jsonOptions);
        },

        // Called by sdkjs when the editor is ready to open a document.
        // NO-OP when there is no pending file: do NOT open an empty doc
        // (that loops: openDocument -> onEndLoadFile -> LocalStartOpen).
        // Guard against firing twice.
        LocalStartOpen: function () {
            if (startOpenDone) return;
            startOpenDone = true;
            setTimeout(function () {
                bridge('pendingFile').then(function (pending) {
                    if (!pending || !pending.handle) {
                        // No file to open. Leave the editor on its start
                        // screen; do not force an empty document.
                        return;
                    }
                    return bridge('convertFile', { path: pending.handle }).then(function (res) {
                        if (!res || !res.data) throw new Error('convertFile returned no data');
                        if (res.name) currentDocName = res.name;
                        var bin = _b64ToBytes(res.data);
                        // INSTRUMENTATION (Kagi round 3): log which loader
                        // path executes. If DesktopOfflineAppDocumentEndLoad
                        // is namespaced / absent, the fallback may itself
                        // fail to construct OpenFileResult — this log is
                        // the first thing to check on a spinner.
                        if (typeof window.DesktopOfflineAppDocumentEndLoad === 'function') {
                            console.log('[jOffice] using DesktopOfflineAppDocumentEndLoad');
                            window.DesktopOfflineAppDocumentEndLoad(pending.name || '', bin, bin.length);
                        } else {
                            console.warn('[jOffice] DesktopOfflineAppDocumentEndLoad MISSING, using fallback openDocument');
                            var ref = getEditor();
                            if (!ref.editor) throw new Error('no editor API');
                            var file = new ref.ew.AscCommon.OpenFileResult();
                            file.data = bin;
                            file.bSerFormat = true;
                            file.url = pending.name || '';
                            ref.editor.openDocument(file);
                        }
                    });
                }).catch(function (e) {
                    console.error('[jOffice] LocalStartOpen failed: ' + (e && e.message ? e.message : e));
                });
            }, 100);
        },

        OpenFilenameDialog: function (filterType, allowMultiple, callback) {
            bridge('openFileDialog', { filterType: filterType || 'any', allowMultiple: !!allowMultiple })
                .then(function (res) {
                    if (res && res.handle && callback) callback(res.handle);
                })
                .catch(function (e) {
                    console.error('[jOffice] OpenFilenameDialog failed: ' + (e && e.message ? e.message : e));
                });
        },

        LocalFileOpen: function (path) {
            // path is an opaque handle from OpenFilenameDialog.
            // Rust resolves it, converts via x2t, returns Editor.bin.
            bridge('openFile', { path: path || null }).then(function (res) {
                if (res && res.data) {
                    var bin = _b64ToBytes(res.data);
                    if (typeof window.DesktopOfflineAppDocumentEndLoad === 'function') {
                        window.DesktopOfflineAppDocumentEndLoad(res.name || '', bin, bin.length);
                    }
                }
            }).catch(function (e) {
                console.error('[jOffice] LocalFileOpen failed: ' + (e && e.message ? e.message : e));
            });
        },

        LocalFileSave: function (param, password, docinfo, fileType, jsonOptions) {
            var isSaveAs = param && param.indexOf('saveas=true') !== -1;
            var ref = getEditor();
            if (!ref.editor) {
                console.error('[jOffice] LocalFileSave aborted: no editor');
                return;
            }
            // INSTRUMENTATION: asc_nativeGetFile must exist or Save
            // hard-fails. Log its presence on first save attempt.
            if (typeof ref.editor.asc_nativeGetFile !== 'function') {
                console.error('[jOffice] asc_nativeGetFile MISSING — save cannot work');
                if (ref.ew && ref.ew.DesktopOfflineAppDocumentEndSave) {
                    ref.ew.DesktopOfflineAppDocumentEndSave(1);
                }
                return;
            }
            try {
                var binData = ref.editor.asc_nativeGetFile();
                if (!binData) {
                    console.error('[jOffice] LocalFileSave aborted: asc_nativeGetFile returned no data');
                    if (ref.ew && ref.ew.DesktopOfflineAppDocumentEndSave) {
                        ref.ew.DesktopOfflineAppDocumentEndSave(1);
                    }
                    return;
                }
                var b64 = (typeof binData === 'string') ? btoa(binData) : _bytesToB64(new Uint8Array(binData));
                // Rust handles the Save As dialog inline when needed.
                bridge('saveFile', { data: b64, saveAs: isSaveAs, fileType: fileType || 0 })
                    .then(function (res) {
                        // User cancelled the Save As dialog: not an error.
                        // Do not call EndSave(1) — the document is unchanged.
                        if (res && res.cancelled) return;
                        if (ref.ew && ref.ew.DesktopOfflineAppDocumentEndSave) {
                            ref.ew.DesktopOfflineAppDocumentEndSave(0);
                        }
                    })
                    .catch(function (e) {
                        console.error('[jOffice] LocalFileSave failed: ' + (e && e.message ? e.message : e));
                        if (ref.ew && ref.ew.DesktopOfflineAppDocumentEndSave) {
                            ref.ew.DesktopOfflineAppDocumentEndSave(1);
                        }
                    });
            } catch (e) {
                console.error('[jOffice] LocalFileSave error: ' + (e && e.message ? e.message : e));
                if (ref.ew && ref.ew.DesktopOfflineAppDocumentEndSave) {
                    ref.ew.DesktopOfflineAppDocumentEndSave(1);
                }
            }
        },

        LocalFileCreate: function (type) {
            return bridge('createNew', { docType: type || 'word' });
        },

        LocalFileGetImageUrl: function (url) {
            if (!url) return url;
            if (url.indexOf('data:') === 0 || url.indexOf('blob:') === 0) return url;
            return url;
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
            bridge('convertFile', { path: filePath, targetFormat: targetFormat || 0 })
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
            bridge('convertFile', { path: file }).then(function (res) {
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
            bridge('convertFile', { path: file }).then(function (res) {
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

    function _b64ToBytes(b64) {
        var raw = atob(b64);
        var bytes = new Uint8Array(raw.length);
        for (var i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
        return bytes;
    }

    function _bytesToB64(bytes) {
        var binary = '';
        for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        return btoa(binary);
    }
})();
