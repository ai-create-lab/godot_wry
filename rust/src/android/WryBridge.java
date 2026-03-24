package org.nicetry.wry;

import android.app.Activity;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.webkit.WebSettings;
import android.widget.FrameLayout;
import android.view.View;
import android.view.ViewGroup;
import java.util.concurrent.CountDownLatch;

public class WryBridge {
    private static WebView sWebView;
    private static Activity sActivity;

    public static void init(Activity activity) {
        sActivity = activity;
    }

    public static void createWebView(final String url) {
        final CountDownLatch latch = new CountDownLatch(1);
        sActivity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    ((ViewGroup) sWebView.getParent()).removeView(sWebView);
                    sWebView.destroy();
                }
                sWebView = new WebView(sActivity);
                WebSettings settings = sWebView.getSettings();
                settings.setJavaScriptEnabled(true);
                settings.setDomStorageEnabled(true);
                settings.setMixedContentMode(WebSettings.MIXED_CONTENT_ALWAYS_ALLOW);
                sWebView.setWebViewClient(new WebViewClient());

                // DecorView の最上位に追加（Vulkan Surface より上）
                ViewGroup decorView = (ViewGroup) sActivity.getWindow().getDecorView();
                FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(
                    FrameLayout.LayoutParams.MATCH_PARENT,
                    FrameLayout.LayoutParams.MATCH_PARENT);
                decorView.addView(sWebView, params);

                sWebView.loadUrl(url);
                latch.countDown();
            }
        });
        try { latch.await(); } catch (InterruptedException e) {}
    }

    public static void loadUrl(final String url) {
        if (sActivity == null || sWebView == null) return;
        sActivity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) sWebView.loadUrl(url);
            }
        });
    }

    public static void setBounds(final int x, final int y, final int w, final int h) {
        if (sActivity == null || sWebView == null) return;
        sActivity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(w, h);
                    params.leftMargin = x;
                    params.topMargin = y;
                    sWebView.setLayoutParams(params);
                }
            }
        });
    }

    public static void setVisible(final boolean visible) {
        if (sActivity == null || sWebView == null) return;
        sActivity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    sWebView.setVisibility(visible ? View.VISIBLE : View.GONE);
                }
            }
        });
    }

    public static void evaluateJavascript(final String script) {
        if (sActivity == null || sWebView == null) return;
        sActivity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) sWebView.evaluateJavascript(script, null);
            }
        });
    }

    public static void destroy() {
        if (sActivity == null || sWebView == null) return;
        sActivity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    ViewGroup parent = (ViewGroup) sWebView.getParent();
                    if (parent != null) parent.removeView(sWebView);
                    sWebView.destroy();
                    sWebView = null;
                }
            }
        });
    }
}
