package dev.mailclient;

import android.app.Activity;
import android.app.Dialog;
import android.net.Uri;
import android.os.Build;
import android.view.Gravity;
import android.view.Window;
import android.view.WindowManager;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;

/**
 * The HTML message view on Android: a WebView laid over the native UI's reader area.
 * It lives in a window of its own: NativeActivity owns its window's surface and input, so a
 * view inside that window would get neither hardware rendering nor touches. The window is a
 * Dialog rather than a PopupWindow because text selection needs an action mode (the Copy
 * toolbar), which only a full window can host.
 * Same policy as the desktop's Servo view (webview/delegate.rs): the message and its inline
 * parts are served from memory under ORIGIN with a CSP, JavaScript is off, and every other
 * request is refused unless remote images were allowed for this message. Links never
 * navigate the view; they go to the app (onLink).
 *
 * Called from Rust on any thread; all view work happens on the UI thread.
 */
public final class MailWebView {
    static final String ORIGIN = "https://message.invalid";

    /** A link was activated: opened in the browser, or the composer for mailto:. */
    static native void onLink(String url);

    /** Back, once the view has no use for it (a selection ends first): handled by the app. */
    static native void onBack();

    private static final class Part {
        final String mime;
        final byte[] data;

        Part(String mime, byte[] data) {
            this.mime = mime;
            this.data = data;
        }
    }

    /** Replaced as a whole: requests are answered on a WebView background thread. */
    private static final class Content {
        final byte[] document;
        final String csp;
        final boolean allowRemote;
        final Map<String, Part> parts;

        Content(byte[] document, String csp, boolean allowRemote, Map<String, Part> parts) {
            this.document = document;
            this.csp = csp;
            this.allowRemote = allowRemote;
            this.parts = parts;
        }
    }

    private static volatile Content content = new Content(new byte[0], "", false, new HashMap<>());
    private static WebView view;
    private static Dialog window;
    private static Activity owner;
    private static int loads;

    public static void show(Activity activity, String document, String csp, boolean allowRemote, String anchor,
            String[] cids, String[] mimes, byte[][] data) {
        Map<String, Part> parts = new HashMap<>();
        for (int i = 0; i < cids.length; i++) {
            parts.put(cids[i], new Part(mimes[i], data[i]));
        }
        Content next = new Content(document.getBytes(StandardCharsets.UTF_8), csp, allowRemote, parts);
        activity.runOnUiThread(() -> {
            content = next;
            WebView v = view(activity);
            v.getSettings().setBlockNetworkLoads(!allowRemote);
            // A fresh URL per message, so the view never shows a cached older one.
            loads++;
            v.loadUrl(ORIGIN + "/msg/" + loads + (anchor.isEmpty() ? "" : "#" + anchor));
        });
    }

    public static void clear(Activity activity) {
        activity.runOnUiThread(() -> {
            content = new Content(new byte[0], "", false, new HashMap<>());
            if (view != null) {
                view.loadUrl("about:blank");
            }
        });
    }

    /** Physical pixels from the screen's top left (the native UI is drawn edge to edge). */
    public static void place(Activity activity, int x, int y, int width, int height, boolean visible) {
        activity.runOnUiThread(() -> {
            Dialog d = window(activity);
            if (!visible || width <= 0 || height <= 0) {
                d.hide();
                return;
            }
            Window w = d.getWindow();
            WindowManager.LayoutParams params = w.getAttributes();
            params.x = x;
            params.y = y;
            params.width = width;
            params.height = height;
            w.setAttributes(params);
            d.show();
        });
    }

    /** A new activity (the old one was destroyed, the process lived on) needs its own window and view. */
    private static void own(Activity activity) {
        if (owner != activity) {
            owner = activity;
            window = null;
            view = null;
        }
    }

    private static Dialog window(Activity activity) {
        own(activity);
        if (window != null) {
            return window;
        }
        Dialog d = new Dialog(activity, android.R.style.Theme_Translucent_NoTitleBar) {
            @Override
            public void onBackPressed() {
                onBack();
            }
        };
        d.setContentView(view(activity));
        d.setCancelable(false);
        Window w = d.getWindow();
        w.setWindowAnimations(0);
        w.clearFlags(WindowManager.LayoutParams.FLAG_DIM_BEHIND);
        // Touches outside go to the native UI below; positions are the screen's, status bar included.
        w.addFlags(WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL
            | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN
            | WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS);
        w.setGravity(Gravity.TOP | Gravity.START);
        if (Build.VERSION.SDK_INT >= 30) {
            w.setDecorFitsSystemWindows(false);
        }
        window = d;
        return d;
    }

    private static WebView view(Activity activity) {
        own(activity);
        if (view != null) {
            return view;
        }
        WebView v = new WebView(activity);
        WebSettings s = v.getSettings();
        s.setJavaScriptEnabled(false);
        s.setAllowFileAccess(false);
        s.setAllowContentAccess(false);
        s.setSupportMultipleWindows(false);
        s.setGeolocationEnabled(false);
        s.setDomStorageEnabled(false);
        s.setBlockNetworkLoads(true);
        s.setSaveFormData(false);
        // Remote images are often plain http; they only load once the user allowed them.
        s.setMixedContentMode(WebSettings.MIXED_CONTENT_ALWAYS_ALLOW);
        // Newsletters are laid out for ~600px: fit them to the screen, pinch to zoom.
        s.setUseWideViewPort(true);
        s.setLoadWithOverviewMode(true);
        s.setBuiltInZoomControls(true);
        s.setDisplayZoomControls(false);
        v.setWebViewClient(new Client());
        view = v;
        return v;
    }

    private static final class Client extends WebViewClient {
        @Override
        public boolean shouldOverrideUrlLoading(WebView v, WebResourceRequest request) {
            String url = request.getUrl().toString();
            // Jumps within the message (#anchors) stay; everything else leaves through the app.
            if (url.startsWith(ORIGIN + "/msg/")) {
                return false;
            }
            onLink(url);
            return true;
        }

        @Override
        public WebResourceResponse shouldInterceptRequest(WebView v, WebResourceRequest request) {
            Content c = content;
            String url = request.getUrl().toString();
            Map<String, String> headers = new HashMap<>();
            headers.put("X-Content-Type-Options", "nosniff");
            if (url.startsWith(ORIGIN + "/msg/")) {
                headers.put("Content-Security-Policy", c.csp);
                return response("text/html", 200, "OK", headers, c.document);
            }
            if (url.startsWith(ORIGIN + "/cid/")) {
                Part part = c.parts.get(Uri.decode(url.substring((ORIGIN + "/cid/").length())));
                if (part != null) {
                    return response(part.mime, 200, "OK", headers, part.data);
                }
            }
            boolean web = url.startsWith("http://") || url.startsWith("https://");
            if (web && c.allowRemote && !url.startsWith(ORIGIN)) {
                return null; // load normally (the CSP still applies)
            }
            return response("text/plain", 403, "Forbidden", headers, new byte[0]);
        }

        private static WebResourceResponse response(String mime, int status, String reason, Map<String, String> headers,
                byte[] body) {
            String charset = mime.startsWith("text/") ? "utf-8" : null;
            return new WebResourceResponse(mime, charset, status, reason, headers, new ByteArrayInputStream(body));
        }
    }
}
