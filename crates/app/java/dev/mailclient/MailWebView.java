package dev.mailclient;

import android.app.Activity;
import android.app.Dialog;
import android.content.Context;
import android.content.res.ColorStateList;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.graphics.drawable.GradientDrawable;
import android.net.Uri;
import android.os.Build;
import android.view.Gravity;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
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

    /** A floating button was tapped: 0 Delete, 1 Archive, 2 Next. */
    static native void onFloat(int action);

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
    private static LinearLayout buttons;
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

    /**
     * The reader's floating buttons, drawn here as the native UI's would be hidden under this window.
     * side: 0 none, 1 right, 2 left; colors are ARGB.
     */
    public static void buttons(Activity activity, int side, int background, int pressed, int foreground, int border) {
        activity.runOnUiThread(() -> {
            window(activity);
            buttons.setVisibility(side == 0 ? View.GONE : View.VISIBLE);
            FrameLayout.LayoutParams lp = (FrameLayout.LayoutParams) buttons.getLayoutParams();
            lp.gravity = Gravity.BOTTOM | (side == 2 ? Gravity.START : Gravity.END);
            buttons.setLayoutParams(lp);
            for (int i = 0; i < buttons.getChildCount(); i++) {
                ((FloatButton) buttons.getChildAt(i)).colors(background, pressed, foreground, border);
            }
        });
    }

    /** A new activity (the old one was destroyed, the process lived on) needs its own window and view. */
    private static void own(Activity activity) {
        if (owner != activity) {
            owner = activity;
            window = null;
            view = null;
            buttons = null;
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
        FrameLayout root = new FrameLayout(activity);
        root.addView(view(activity));
        float dp = activity.getResources().getDisplayMetrics().density;
        LinearLayout column = new LinearLayout(activity);
        column.setOrientation(LinearLayout.VERTICAL);
        String[] labels = {"Delete", "Archive", "Next"};
        for (int i = 0; i < labels.length; i++) {
            int action = i;
            FloatButton b = new FloatButton(activity, i);
            b.setContentDescription(labels[i]);
            b.setOnClickListener(v -> onFloat(action));
            LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(Math.round(56 * dp), Math.round(56 * dp));
            lp.topMargin = i == 0 ? 0 : Math.round(12 * dp);
            column.addView(b, lp);
        }
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT,
            FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM | Gravity.END);
        lp.setMargins(Math.round(16 * dp), 0, Math.round(16 * dp), Math.round(16 * dp));
        column.setVisibility(View.GONE);
        root.addView(column, lp);
        buttons = column;
        d.setContentView(root);
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

    /** The native UI's FloatButton (widgets.slint): a round button with a Lucide icon. */
    private static final class FloatButton extends View {
        private final Path icon = new Path();
        private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
        private final GradientDrawable background = new GradientDrawable();

        FloatButton(Context context, int which) {
            super(context);
            float dp = context.getResources().getDisplayMetrics().density;
            background.setShape(GradientDrawable.OVAL);
            setBackground(background);
            setElevation(4 * dp);
            setClickable(true);
            paint.setStyle(Paint.Style.STROKE);
            paint.setStrokeWidth(2);
            paint.setStrokeCap(Paint.Cap.ROUND);
            paint.setStrokeJoin(Paint.Join.ROUND);
            // The icons' SVG paths (ui/icons), on their 24-unit grid.
            Path p = icon;
            if (which == 0) { // trash.svg
                p.moveTo(10, 11); p.lineTo(10, 17);
                p.moveTo(14, 11); p.lineTo(14, 17);
                p.moveTo(19, 6); p.lineTo(19, 20); p.arcTo(new RectF(15, 18, 19, 22), 0, 90);
                p.lineTo(7, 22); p.arcTo(new RectF(5, 18, 9, 22), 90, 90); p.lineTo(5, 6);
                p.moveTo(3, 6); p.lineTo(21, 6);
                p.moveTo(8, 6); p.lineTo(8, 4); p.arcTo(new RectF(8, 2, 12, 6), 180, 90);
                p.lineTo(14, 2); p.arcTo(new RectF(12, 2, 16, 6), 270, 90); p.lineTo(16, 6);
            } else if (which == 1) { // archive.svg
                p.addRoundRect(new RectF(2, 3, 22, 8), 1, 1, Path.Direction.CW);
                p.moveTo(4, 8); p.lineTo(4, 19); p.arcTo(new RectF(4, 17, 8, 21), 180, -90);
                p.lineTo(18, 21); p.arcTo(new RectF(16, 17, 20, 21), 90, -90); p.lineTo(20, 8);
                p.moveTo(10, 12); p.lineTo(14, 12);
            } else { // arrow-down.svg
                p.moveTo(12, 5); p.lineTo(12, 19);
                p.moveTo(19, 12); p.lineTo(12, 19); p.lineTo(5, 12);
            }
        }

        void colors(int fill, int pressed, int foreground, int border) {
            int[][] states = {{android.R.attr.state_pressed}, {}};
            background.setColor(new ColorStateList(states, new int[] {pressed, fill}));
            background.setStroke(Math.round(getResources().getDisplayMetrics().density), border);
            paint.setColor(foreground);
            invalidate();
        }

        @Override
        public void setPressed(boolean pressed) {
            super.setPressed(pressed);
            // Pressed, the button sinks, as the native UI's does.
            animate().scaleX(pressed ? 0.92f : 1f).scaleY(pressed ? 0.92f : 1f).setDuration(pressed ? 60 : 220).start();
        }

        @Override
        protected void onDraw(Canvas canvas) {
            float size = 22 * getResources().getDisplayMetrics().density;
            canvas.translate((getWidth() - size) / 2, (getHeight() - size) / 2);
            canvas.scale(size / 24, size / 24);
            canvas.drawPath(icon, paint);
        }
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
