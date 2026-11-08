// The Android app of a Ferritron plugin: the plugin (libferritron_app.so)
// plays through AAudio; its web editor (assets/ui), or one made from its
// parameters (assets/ferritron/editor.html), runs in a WebView and talks to
// the plugin through `ferritronAndroid` (assets/ferritron/bridge.js gives
// pages the same `window.ferritron` as inside a DAW). MIDI devices play
// instruments.
//
// SPDX-License-Identifier: MIT OR Apache-2.0
package dev.ferritron.app;

import android.Manifest;
import android.app.Activity;
import android.content.pm.PackageManager;
import android.media.midi.MidiDevice;
import android.media.midi.MidiDeviceInfo;
import android.media.midi.MidiManager;
import android.media.midi.MidiOutputPort;
import android.media.midi.MidiReceiver;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.webkit.JavascriptInterface;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

public class FerritronActivity extends Activity {
    static {
        System.loadLibrary("ferritron_app");
    }

    static native long nativeStart(String dataDir, String language, boolean record);
    static native void nativeSuspend(long handle, boolean suspend);
    static native void nativeStop(long handle);
    static native String nativeDescribe(long handle);
    static native String nativeValues(long handle);
    static native void nativeSet(long handle, int id, double plain);
    static native void nativeMidi(long handle, byte[] data, int offset, int count);

    /** The virtual origin assets are served from (reserved for this use by Android). */
    private static final String HOST = "appassets.androidplatform.net";

    private static String mime(String path) {
        if (path.endsWith(".html") || path.endsWith(".htm")) return "text/html";
        if (path.endsWith(".js") || path.endsWith(".mjs")) return "text/javascript";
        if (path.endsWith(".css")) return "text/css";
        if (path.endsWith(".json")) return "application/json";
        if (path.endsWith(".svg")) return "image/svg+xml";
        if (path.endsWith(".png")) return "image/png";
        if (path.endsWith(".jpg") || path.endsWith(".jpeg")) return "image/jpeg";
        if (path.endsWith(".woff2")) return "font/woff2";
        if (path.endsWith(".wasm")) return "application/wasm";
        return "application/octet-stream";
    }

    private volatile long handle;
    private boolean suspended;
    private WebView web;
    private boolean instrument;
    private final List<MidiDevice> midiDevices = new ArrayList<>();
    private final Handler main = new Handler(Looper.getMainLooper());

    /** What pages call as `ferritronAndroid` (bridge.js). */
    public class Bridge {
        @JavascriptInterface
        public String describe() {
            return nativeDescribe(handle);
        }

        @JavascriptInterface
        public String values() {
            long h = handle;
            return h == 0 ? "[]" : nativeValues(h);
        }

        /** `set:<id>:<plain value>` or `midi:<status>:<data1>:<data2>`. */
        @JavascriptInterface
        public void post(String message) {
            long h = handle;
            if (h == 0 || message == null) return;
            String[] parts = message.split(":");
            try {
                if (parts[0].equals("set") && parts.length == 3) {
                    nativeSet(h, Integer.parseInt(parts[1]), Double.parseDouble(parts[2]));
                } else if (parts[0].equals("midi") && parts.length == 4) {
                    byte[] bytes = { (byte) Integer.parseInt(parts[1]), (byte) Integer.parseInt(parts[2]), (byte) Integer.parseInt(parts[3]) };
                    nativeMidi(h, bytes, 0, 3);
                }
            } catch (NumberFormatException e) {
                // ignore malformed messages
            }
        }
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        instrument = nativeDescribe(0).contains("\"kind\":\"instrument\"");
        web = new WebView(this);
        setContentView(web);
        WebSettings settings = web.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setAllowFileAccess(true);
        settings.setDomStorageEnabled(true);
        web.addJavascriptInterface(new Bridge(), "ferritronAndroid");
        web.setWebViewClient(new WebViewClient() {
            @Override
            public WebResourceResponse shouldInterceptRequest(WebView view, WebResourceRequest request) {
                // Assets are served from a virtual origin (WebView does not let
                // file:///android_asset pages be changed); the plugin's own
                // page gets the bridge before its scripts run.
                if (!HOST.equals(request.getUrl().getHost())) return null;
                String path = request.getUrl().getPath();
                if (path == null || path.equals("/")) path = "/ui/index.html";
                try {
                    byte[] data = read(path.substring(1));
                    if (path.equals("/ui/index.html")) {
                        String html = new String(data, StandardCharsets.UTF_8);
                        int head = html.indexOf("<head");
                        int at = head < 0 ? 0 : html.indexOf('>', head) + 1;
                        html = html.substring(0, at) + "<script src=\"/ferritron/bridge.js\"></script>" + html.substring(at);
                        data = html.getBytes(StandardCharsets.UTF_8);
                    }
                    return new WebResourceResponse(mime(path), "utf-8", new ByteArrayInputStream(data));
                } catch (IOException e) {
                    return new WebResourceResponse("text/plain", "utf-8", 404, "Not Found", null, new ByteArrayInputStream(new byte[0]));
                }
            }
        });
        boolean ownEditor;
        try (InputStream in = getAssets().open("ui/index.html")) {
            ownEditor = true;
        } catch (IOException e) {
            ownEditor = false;
        }
        web.loadUrl("https://" + HOST + (ownEditor ? "/ui/index.html" : "/ferritron/editor.html"));
        if (!instrument && checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] { Manifest.permission.RECORD_AUDIO }, 1);
        } else {
            start(!instrument);
        }
        openMidi();
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] permissions, int[] results) {
        start(results.length > 0 && results[0] == PackageManager.PERMISSION_GRANTED);
    }

    private void start(boolean record) {
        if (handle == 0) handle = nativeStart(getFilesDir().getAbsolutePath(), Locale.getDefault().toLanguageTag(), record);
    }

    private byte[] read(String asset) throws IOException {
        try (InputStream in = getAssets().open(asset)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buffer = new byte[8192];
            int n;
            while ((n = in.read(buffer)) > 0) out.write(buffer, 0, n);
            return out.toByteArray();
        }
    }

    private void openMidi() {
        if (!getPackageManager().hasSystemFeature(PackageManager.FEATURE_MIDI)) return;
        MidiManager manager = (MidiManager) getSystemService(MIDI_SERVICE);
        if (manager == null) return;
        for (MidiDeviceInfo info : manager.getDevices()) openMidi(manager, info);
        manager.registerDeviceCallback(new MidiManager.DeviceCallback() {
            @Override
            public void onDeviceAdded(MidiDeviceInfo info) {
                openMidi(manager, info);
            }
        }, main);
    }

    private void openMidi(MidiManager manager, MidiDeviceInfo info) {
        if (info.getOutputPortCount() == 0) return;
        manager.openDevice(info, device -> {
            if (device == null) return;
            midiDevices.add(device);
            for (int i = 0; i < info.getOutputPortCount(); i++) {
                MidiOutputPort port = device.openOutputPort(i);
                if (port == null) continue;
                port.connect(new MidiReceiver() {
                    @Override
                    public void onSend(byte[] data, int offset, int count, long timestamp) {
                        long h = handle;
                        if (h != 0) nativeMidi(h, data, offset, count);
                    }
                });
            }
        }, main);
    }

    @Override
    protected void onPause() {
        super.onPause();
        if (handle != 0) nativeSuspend(handle, true);
        suspended = true;
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (handle != 0 && suspended) nativeSuspend(handle, false);
        suspended = false;
    }

    @Override
    protected void onDestroy() {
        if (handle != 0) nativeStop(handle);
        handle = 0;
        for (MidiDevice device : midiDevices) {
            try {
                device.close();
            } catch (IOException e) {
                // closing anyway
            }
        }
        super.onDestroy();
    }
}
