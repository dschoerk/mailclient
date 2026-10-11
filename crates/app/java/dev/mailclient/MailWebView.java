package dev.mailclient;

import android.app.Activity;
import android.app.Dialog;
import android.content.Context;
import android.content.res.ColorStateList;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.graphics.Color;
import android.graphics.drawable.GradientDrawable;
import android.net.Uri;
import android.os.Build;
import android.print.PrintAttributes;
import android.print.PrintManager;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowManager;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
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
 * A conversation is a native stack with one WebView per message, so the scroll position maps to
 * a message without running any script in the page (onScrolledTo).
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
    /** The user scrolled the conversation to this message (the one under the reading line). */
    static native void onScrolledTo(int index);

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
        final byte[][] documents;
        final String csp;
        final boolean allowRemote;
        final Map<String, Part> parts;

        Content(byte[][] documents, String csp, boolean allowRemote, Map<String, Part> parts) {
            this.documents = documents;
            this.csp = csp;
            this.allowRemote = allowRemote;
            this.parts = parts;
        }
    }

    private static volatile Content content = new Content(new byte[0][], "", false, new HashMap<>());
    /** A lone message: one view filling the window, scrolling and zooming itself. */
    private static WebView view;
    /** The open conversation, or null while a lone message is shown. */
    private static Conversation conversation;
    private static Dialog window;
    private static LinearLayout buttons;
    /** The window's content under the buttons: the lone message's view or the conversation. */
    private static FrameLayout body;
    private static Activity owner;
    private static int loads;

    /** documents: one per message, "" while it loads; headings: one per message, none for a lone message. */
    public static void show(Activity activity, String[] documents, String[] headings, int current, String csp,
            boolean allowRemote, String[] cids, String[] mimes, byte[][] data) {
        Map<String, Part> parts = new HashMap<>();
        for (int i = 0; i < cids.length; i++) {
            parts.put(cids[i], new Part(mimes[i], data[i]));
        }
        byte[][] bytes = new byte[documents.length][];
        for (int i = 0; i < documents.length; i++) {
            bytes[i] = documents[i].getBytes(StandardCharsets.UTF_8);
        }
        Content next = new Content(bytes, csp, allowRemote, parts);
        activity.runOnUiThread(() -> {
            content = next;
            // A fresh URL per message, so a view never shows a cached older one.
            loads++;
            Dialog d = window(activity);
            dropConversation();
            if (headings.length < 2) {
                WebView v = view(activity);
                v.getSettings().setBlockNetworkLoads(!allowRemote);
                body(v);
                v.loadUrl(ORIGIN + "/msg/" + loads + "/0");
            } else {
                conversation = new Conversation(activity, documents, headings, current, allowRemote);
                body(conversation.scroll);
                if (view != null) {
                    view.loadUrl("about:blank");
                }
            }
        });
    }

    /**
     * The system print dialog for the open message, laid out by its WebView as shown; the dialog
     * also offers Save as PDF. Nothing happens while the message is still loading.
     */
    public static void print(Activity activity, String title) {
        activity.runOnUiThread(() -> {
            WebView v = conversation != null ? conversation.views[conversation.current] : view;
            if (v == null) {
                return;
            }
            PrintManager printer = (PrintManager) activity.getSystemService(Context.PRINT_SERVICE);
            printer.print(title, v.createPrintDocumentAdapter(title), new PrintAttributes.Builder().build());
        });
    }

    public static void clear(Activity activity) {
        activity.runOnUiThread(() -> {
            content = new Content(new byte[0][], "", false, new HashMap<>());
            dropConversation();
            WebView v = view(activity);
            window(activity);
            body(v);
            v.loadUrl("about:blank");
        });
    }

    /** Shows v in the window, under the floating buttons. */
    private static void body(View v) {
        if (v.getParent() != body) {
            body.removeAllViews();
            body.addView(v);
        }
    }

    private static void dropConversation() {
        if (conversation == null) {
            return;
        }
        for (WebView v : conversation.views) {
            if (v != null) {
                ((ViewGroup) v.getParent()).removeView(v);
                v.destroy();
            }
        }
        conversation = null;
    }

    /**
     * A conversation: each message's heading and WebView, as tall as its content, stacked in a
     * native ScrollView, with a bar marking the open message. Once the user scrolls, the bar moves to
     * the message under a reading line that slides from the top of the view to its bottom as the
     * stack scrolls from start to end, so every message is reached, however short. Same rule as the
     * desktop (mailcore::mime::SCROLLED_SCRIPT).
     */
    private static final class Conversation {
        static final int ACCENT = 0xff2f6fde;
        static final int DIVIDER = 0xffe0e0e3;

        final ScrollView scroll;
        final LinearLayout stack;
        final View[] rows;
        final View[] bars;
        final WebView[] views;
        /** The open message. */
        final int current;
        int marked;
        /** The user touched the stack: it no longer follows the open message as views grow. */
        boolean touched;

        Conversation(Activity activity, String[] documents, String[] headings, int current, boolean allowRemote) {
            float dp = activity.getResources().getDisplayMetrics().density;
            int n = headings.length;
            rows = new View[n];
            bars = new View[n];
            views = new WebView[n];
            this.current = current;
            marked = current;
            scroll = new ScrollView(activity);
            scroll.setBackgroundColor(Color.WHITE);
            stack = new LinearLayout(activity);
            stack.setOrientation(LinearLayout.VERTICAL);
            GradientDrawable divider = new GradientDrawable();
            divider.setColor(DIVIDER);
            divider.setSize(1, 1);
            stack.setDividerDrawable(divider);
            stack.setShowDividers(LinearLayout.SHOW_DIVIDER_MIDDLE);
            for (int i = 0; i < n; i++) {
                LinearLayout row = new LinearLayout(activity);
                View bar = new View(activity);
                bar.setBackgroundColor(i == current ? ACCENT : DIVIDER);
                row.addView(bar, new LinearLayout.LayoutParams((int) (3 * dp), ViewGroup.LayoutParams.MATCH_PARENT));
                LinearLayout column = new LinearLayout(activity);
                column.setOrientation(LinearLayout.VERTICAL);
                TextView heading = new TextView(activity);
                heading.setText(headings[i]);
                heading.setTextColor(0xff6e6e73);
                heading.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
                heading.setPadding((int) (16 * dp), (int) (12 * dp), (int) (16 * dp), 0);
                column.addView(heading);
                if (documents[i].isEmpty()) {
                    TextView loading = new TextView(activity);
                    loading.setText("Loading…");
                    loading.setTextColor(0xff6e6e73);
                    loading.setPadding((int) (16 * dp), (int) (12 * dp), (int) (16 * dp), (int) (12 * dp));
                    column.addView(loading);
                } else {
                    WebView v = newView(activity);
                    v.getSettings().setBlockNetworkLoads(!allowRemote);
                    v.setVerticalScrollBarEnabled(false);
                    column.addView(v, new LinearLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
                    v.loadUrl(ORIGIN + "/msg/" + loads + "/" + i);
                    views[i] = v;
                }
                row.addView(column, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1));
                stack.addView(row);
                rows[i] = row;
                bars[i] = bar;
            }
            scroll.addView(stack);
            // Views grow as their pages load: keep the open message at the top until the user takes over.
            stack.addOnLayoutChangeListener((v, l, t, r, b, ol, ot, or, ob) -> {
                if (!touched) {
                    scroll.scrollTo(0, rows[current].getTop());
                }
            });
            // Reaches the stack only once it scrolls (a drag the WebViews leave to it), not for taps on links.
            scroll.setOnTouchListener((v, e) -> {
                touched = true;
                return false;
            });
            scroll.setOnScrollChangeListener((v, x, y, ox, oy) -> {
                int max = stack.getHeight() - scroll.getHeight();
                if (touched && max > 0) {
                    mark(y + (float) y / max * scroll.getHeight());
                }
            });
        }

        private void mark(float line) {
            int n = 0;
            for (int i = 0; i < rows.length; i++) {
                if (rows[i].getTop() <= line) {
                    n = i;
                }
            }
            if (n == marked) {
                return;
            }
            bars[marked].setBackgroundColor(DIVIDER);
            bars[n].setBackgroundColor(ACCENT);
            marked = n;
            onScrolledTo(n);
        }
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
            body = null;
            conversation = null;
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
        body = new FrameLayout(activity);
        body.addView(view(activity));
        root.addView(body);
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
        WebView v = newView(activity);
        WebSettings s = v.getSettings();
        // Newsletters are laid out for ~600px: fit them to the screen, pinch to zoom.
        s.setUseWideViewPort(true);
        s.setLoadWithOverviewMode(true);
        s.setBuiltInZoomControls(true);
        s.setDisplayZoomControls(false);
        view = v;
        return v;
    }

    /** A view with the message policy: no scripts, no storage, requests answered by Client. */
    private static WebView newView(Activity activity) {
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
        v.setWebViewClient(new Client());
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
                // /msg/<load>/<message>
                String path = request.getUrl().getPath();
                int i = -1;
                try {
                    i = Integer.parseInt(path.substring(path.lastIndexOf('/') + 1));
                } catch (NumberFormatException e) {
                    // falls through to 404
                }
                if (i >= 0 && i < c.documents.length) {
                    headers.put("Content-Security-Policy", c.csp);
                    return response("text/html", 200, "OK", headers, c.documents[i]);
                }
                return response("text/plain", 404, "Not Found", headers, new byte[0]);
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
