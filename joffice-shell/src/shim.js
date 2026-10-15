// AscDesktopEditor host shim for jOffice.
//
// sdkjs does NOT talk to window.joffice.invoke. It talks to
// window.AscDesktopEditor — the desktop-host interface that ONLYOFFICE
// DesktopEditors and euro-office-lite implement. This shim implements
// that interface on top of the Rust bridge (window.joffice.invoke).
//
// Flow for opening a local file:
//   1. sdkjs calls AscDesktopEditor.LocalFileOpen(path)
//   2. Shim asks Rust to convert the file via x2t -> Editor.bin (base64)
//   3. Shim feeds Editor.bin to the editor via openDocument()
// Flow for saving:
//   1. sdkjs calls AscDesktopEditor.LocalFileSave(...)
//   2. Shim pulls Editor.bin from the editor via asc_nativeGetFile()
//   3. Shim asks Rust to convert Editor.bin back via x2t and write it
//
// This file is injected into the TOP frame only, before any page script,
// on the joffice:// origin. It must not run on any other origin.

(function () {
    'use strict';

    // The Rust bridge. Every call returns a Promise.
    function bridge(cmd, args) {
        return window.webkit.messageHandlers.joffice.postMessage({ cmd: cmd, args: args || {} });
    }

    // Find the editor API object (lives in the editor iframe).
    function getEditor() {
        var frames = document.querySelectorAll('iframe');
        for (var i = 0; i < frames.length; i++) {
            try {
                if (frames[i].contentWindow && frames[i].contentWindow.Asc) {
                    return { editor: frames[i].contentWindow.Asc.editor || null, ew: frames[i].contentWindow };
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
        LocalFileGetSourcePath: function () { return ''; },
        LocalFileGetSaved: function () { return false; },

        getFontsSprite: function (suffix) {
            suffix = suffix || '';
            return 'joffice:///sdkjs/common/Images/fonts_thumbnail' + suffix + '.png';
        },

        CreateEditorApi: function (api) {
            // sdkjs hands us the editor API; stash the editor window.
            try {
                var frames = document.querySelectorAll('iframe');
                for (var i = 0; i < frames.length; i++) {
                    try {
                        if (frames[i].contentWindow && frames[i].contentWindow.Asc) {
                            window.AscDesktopEditor._editorWindow = frames[i].contentWindow;
                            break;
                        }
                    } catch (e) { /* skip */ }
                }
            } catch (e) { /* ignore */ }
        },

        // Called by sdkjs when the editor is ready to open a document.
        LocalStartOpen: function () {
            var ref = getEditor();
            if (!ref.editor) return;
            setTimeout(function () {
                try {
                    // Ask Rust: is there a pending file to open?
                    bridge('pendingFile').then(function (pending) {
                        if (pending && pending.path) {
                            // Convert via x2t, then feed Editor.bin to the editor.
                            return bridge('convertFile', { path: pending.path }).then(function (res) {
                                if (!res || !res.data) throw new Error('convertFile returned no data');
                                var bin = _b64ToBytes(res.data);
                                var file = new ref.ew.AscCommon.OpenFileResult();
                                file.data = bin;
                                file.bSerFormat = true;
                                ref.editor.openDocument(file);
                                ref.ew.AscCommon.History.UserSaveMode = true;
                            });
                        }
                        // No file: open an empty document.
                        var emptyData = ref.ew.AscCommon.getEmpty();
                        var file = new ref.ew.AscCommon.OpenFileResult();
                        file.data = emptyData;
                        file.bSerFormat = true;
                        ref.editor.openDocument(file);
                        ref.ew.AscCommon.History.UserSaveMode = true;
                    }).catch(function (e) {
                        console.error('[jOffice] LocalStartOpen failed: ' + (e && e.message ? e.message : e));
                    });
                } catch (e) {
                    console.error('[jOffice] LocalStartOpen error: ' + (e && e.message ? e.message : e));
                }
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
            // path may be an opaque handle from OpenFilenameDialog, or a real path.
            // Either way, Rust resolves it. We just trigger the open flow:
            // convert via x2t and feed Editor.bin, or reload if a doc is open.
            bridge('openFile', { path: path || null }).then(function (res) {
                if (res && res.data) {
                    var ref = getEditor();
                    if (!ref.editor) return;
                    var bin = _b64ToBytes(res.data);
                    var file = new ref.ew.AscCommon.OpenFileResult();
                    file.data = bin;
                    file.bSerFormat = true;
                    ref.editor.openDocument(file);
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
            try {
                var binData = ref.editor.asc_nativeGetFile();
                if (!binData) {
                    console.error('[jOffice] LocalFileSave aborted: asc_nativeGetFile returned no data');
                    return;
                }
                var b64 = (typeof binData === 'string') ? btoa(binData) : _bytesToB64(new Uint8Array(binData));
                bridge('saveFile', { data: b64, saveAs: isSaveAs, fileType: fileType || 0 })
                    .then(function () {
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

        // Compare / merge delegate to the same convert path.
        CompareDocumentFile: function (file, oOptions) {
            bridge('convertFile', { path: file }).then(function (res) {
                var ref = getEditor();
                if (ref.ew && ref.ew.onDocumentCompare) {
                    ref.ew.onDocumentCompare('', res.data, res.data.length, res.images || {}, oOptions);
                }
            }).catch(function (e) {
                console.error('[jOffice] CompareDocumentFile failed: ' + (e && e.message ? e.message : e));
            });
        },

        MergeDocumentFile: function (file, oOptions) {
            bridge('convertFile', { path: file }).then(function (res) {
                var ref = getEditor();
                if (ref.ew && ref.ew.onDocumentMerge) {
                    ref.ew.onDocumentMerge('', res.data, res.data.length, res.images || {}, oOptions);
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
