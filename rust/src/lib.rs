mod godot_window;
mod protocols;

use godot::global::MouseButtonMask;
use godot::init::*;
use godot::prelude::*;
use godot::classes::{Control, DisplayServer, IControl, Input, InputEventMouseButton, InputEventMouseMotion, InputEventKey, ProjectSettings};
use godot::global::{Key, MouseButton};
use lazy_static::lazy_static;
use serde_json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use wry::{WebViewBuilder, WebContext, Rect, WebViewAttributes, PageLoadEvent};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::http::Request;

use crate::godot_window::GodotWindow;
use crate::protocols::get_res_response;

#[cfg(target_os = "windows")]
use {
    raw_window_handle::{HasWindowHandle, RawWindowHandle},
    windows::Win32::Foundation::HWND,
    windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrA, SetWindowLongPtrA, GWL_STYLE},
};

// Required for Windows to link against the wevtapi library for webview2,
// not sure why webview2-com-sys doesn't do this automatically.
#[cfg(target_os = "windows")]
#[link(name = "wevtapi")]
extern "system" {}



struct GodotWRY;

#[gdextension]
unsafe impl ExtensionLibrary for GodotWRY {}

#[derive(GodotClass)]
#[class(base=Control)]
struct WebView {
    base: Base<Control>,
    webview: Option<wry::WebView>,
    previous_screen_position: Vector2,
    previous_viewport_size: Vector2i,
    previous_window_position: Vector2i,
    #[export]
    full_window_size: bool,
    #[export]
    url: GString,
    #[export]
    html: GString,
    #[export]
    data_directory: GString,
    #[export]
    transparent: bool,
    #[export]
    background_color: Color,
    #[export]
    devtools: bool,
    #[export]
    headers: Dictionary,
    #[export]
    user_agent: GString,
    #[export]
    zoom_hotkeys: bool,
    #[export]
    clipboard: bool,
    #[export]
    incognito: bool,
    #[export]
    focused_when_created: bool,
    #[export]
    forward_input_events: bool,
    #[export]
    autoplay: bool,
}

#[godot_api]
impl IControl for WebView {
    fn init(base: Base<Control>) -> Self {
        Self {
            base,
            webview: None,
            previous_screen_position: Vector2::default(),
            previous_viewport_size: Vector2i::default(),
            previous_window_position: Vector2i::default(),
            full_window_size: true,
            url: "https://github.com/doceazedo/godot_wry".into(),
            html: "".into(),
            data_directory: "user://".into(),
            transparent: false,
            background_color: Color::from_rgb(1.0, 1.0, 1.0),
            devtools: true,
            headers: Dictionary::new(),
            user_agent: "".into(),
            zoom_hotkeys: false,
            clipboard: true,
            incognito: false,
            focused_when_created: true,
            forward_input_events: true,
            autoplay: false,
        }
    }

    fn ready(&mut self) {
        self.create_webview();
    }

    fn process(&mut self, _delta: f64) {
        self.update_webview();
    }
}

#[godot_api]
impl WebView {
    #[signal]
    fn ipc_message(message: GString);

    #[signal]
    fn page_load_started(message: GString);

    #[signal]
    fn page_load_finished(message: GString);

    #[func]
    fn update_webview(&mut self) {
        if let Some(_) = &self.webview {
            let viewport_size = self.base().get_tree().expect("Could not get tree").get_root().expect("Could not get viewport").get_size();
            let window_position = DisplayServer::singleton().window_get_position();

            let needs_resize = self.base().get_screen_position() != self.previous_screen_position
                || viewport_size != self.previous_viewport_size
                || window_position != self.previous_window_position;

            if needs_resize {
                self.previous_screen_position = self.base().get_screen_position();
                self.previous_viewport_size = viewport_size;
                self.previous_window_position = window_position;
                self.resize();
            }

            #[cfg(target_os = "linux")]
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
        }
    }

    #[func]
    fn create_webview(&mut self) {
        let display_server = DisplayServer::singleton();
        if display_server.get_name() == "headless".into()
        {
            godot_warn!("Godot WRY: Headless mode detected. webview will not be created.");
            return;
        }

        #[cfg(target_os = "linux")]
        gtk::init().expect("Failed to initialize GTK");

        // Android: WRY の build_as_child は Android 非対応のため、DEX ブリッジ経由で直接 WebView を作成
        #[cfg(target_os = "android")]
        {
            match create_android_webview(&String::from(&self.url)) {
                Ok(()) => {
                    godot_print!("[Godot WRY] Android WebView created via bridge");
                    // Android では wry::WebView は使わないが、resize/visibility は JNI 経由で行う
                }
                Err(e) => {
                    godot_error!("[Godot WRY] Android WebView creation failed: {}", e);
                }
            }
            return;
        }

        let window = GodotWindow;

        // remove WS_CLIPCHILDREN from the window style
        // otherwise, transparent on windows won't work
        #[cfg(target_os = "windows")]
        {
            let handle = window.window_handle().unwrap().as_raw();
            let raw_handle: HWND = match handle {
                RawWindowHandle::Win32(win32) => HWND(win32.hwnd.get() as _),
                _ => {
                    panic!("Unsupported window handle type");
                }
            };

            unsafe {
                let current_style = GetWindowLongPtrA(raw_handle, GWL_STYLE);
                // remove WS_CLIPCHILDREN
                SetWindowLongPtrA(raw_handle, GWL_STYLE, current_style & !0x02000000);
            };
        }

        let base = Arc::new(Mutex::new(self.base().clone()));
        let resolved_data_directory: Option<PathBuf> = if !self.data_directory.is_empty() {
            let data_directory = self.data_directory.to_string();

            if data_directory.starts_with("user://") {
                let path_without_prefix = data_directory.trim_start_matches("user://");

                let project_settings = ProjectSettings::singleton();
                let base_path = project_settings.globalize_path("user://").to_string();
                let mut absolute_path = PathBuf::from(base_path);
                absolute_path.push(path_without_prefix);

                std::fs::create_dir_all(&absolute_path).ok();

                Some(absolute_path)
            } else {
                let path = PathBuf::from(&data_directory);
                std::fs::create_dir_all(&path).ok();
                Some(path)
            }
        } else {
            None
        };
        let mut context = WebContext::new(resolved_data_directory);
        let webview_builder = WebViewBuilder::with_attributes(WebViewAttributes {
            context: Some(&mut context),
            url: if self.html.is_empty() { Some(String::from(&self.url)) } else { None },
            html: if self.url.is_empty() { Some(String::from(&self.html)) } else { None },
            transparent: self.transparent,
            devtools: self.devtools,
            // headers: Some(HeaderMap::try_from(self.headers.iter_shared().typed::<GString, Variant>()).unwrap_or_default()),
            user_agent: Some(String::from(&self.user_agent)),
            zoom_hotkeys_enabled: self.zoom_hotkeys,
            clipboard: self.clipboard,
            incognito: self.incognito,
            focused: self.focused_when_created,
            autoplay: self.autoplay,
            accept_first_mouse: true,
            ..Default::default()
        })
            .with_ipc_handler({
                let base = Arc::clone(&base);
                move |req: Request<String>| {
                    let mut base = base.lock().unwrap();
                    let body = req.body().as_str();
                    
                    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(body) {
                        if let Some(event_type) = json_value.get("type").and_then(|t| t.as_str()) {
                            match event_type {
                                "_mouse_move" => {
                                    let x = json_value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let y = json_value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    
                                    let movement_x = json_value.get("movementX").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let movement_y = json_value.get("movementY").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    
                                    let mut event = InputEventMouseMotion::new_gd();
                                    event.set_position(Vector2::new(x, y));
                                    event.set_global_position(Vector2::new(x, y));
                                    
                                    let button_mask = CURRENT_BUTTON_MASK.lock().unwrap();
                                    event.set_button_mask(*button_mask);

                                    event.set_relative(Vector2::new(movement_x, movement_y));
                                    
                                    Input::singleton().parse_input_event(&event);
                                    return;
                                },
                                
                                "_mouse_down" | "_mouse_up" => {
                                    let x = json_value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let y = json_value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let button = json_value.get("button").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                                    
                                    let godot_button = match button {
                                        0 => MouseButton::LEFT,
                                        1 => MouseButton::MIDDLE,
                                        2 => MouseButton::RIGHT,
                                        3 => MouseButton::WHEEL_UP,
                                        4 => MouseButton::WHEEL_DOWN,
                                        _ => MouseButton::LEFT, // default to left button
                                    };
                                    
                                    let pressed = event_type == "_mouse_down";
                                    let mask = match godot_button {
                                        MouseButton::LEFT => MouseButtonMask::LEFT,
                                        MouseButton::RIGHT => MouseButtonMask::RIGHT,
                                        MouseButton::MIDDLE => MouseButtonMask::MIDDLE,
                                        _ => MouseButtonMask::default(),
                                    };
                                    
                                    if godot_button != MouseButton::WHEEL_UP && godot_button != MouseButton::WHEEL_DOWN {
                                        let mut button_mask = CURRENT_BUTTON_MASK.lock().unwrap();
                                        if pressed {
                                            *button_mask = *button_mask | mask;
                                        } else {
                                            match godot_button {
                                                MouseButton::LEFT => {
                                                    if button_mask.is_set(MouseButtonMask::LEFT) {
                                                        *button_mask = MouseButtonMask::from_ord(button_mask.ord() & !MouseButtonMask::LEFT.ord());
                                                    }
                                                },
                                                MouseButton::RIGHT => {
                                                    if button_mask.is_set(MouseButtonMask::RIGHT) {
                                                        *button_mask = MouseButtonMask::from_ord(button_mask.ord() & !MouseButtonMask::RIGHT.ord());
                                                    }
                                                },
                                                MouseButton::MIDDLE => {
                                                    if button_mask.is_set(MouseButtonMask::MIDDLE) {
                                                        *button_mask = MouseButtonMask::from_ord(button_mask.ord() & !MouseButtonMask::MIDDLE.ord());
                                                    }
                                                },
                                                _ => {}
                                            }
                                        }
                                    }
                                    
                                    let mut event = InputEventMouseButton::new_gd();
                                    event.set_button_index(godot_button);
                                    event.set_position(Vector2::new(x, y));
                                    event.set_global_position(Vector2::new(x, y));
                                    event.set_pressed(pressed);
                                    
                                    let button_mask = CURRENT_BUTTON_MASK.lock().unwrap();
                                    event.set_button_mask(*button_mask);
                                    
                                    Input::singleton().parse_input_event(&event);
                                    return;
                                },

                                "_mouse_wheel" => {
                                    let x = json_value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let y = json_value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let delta_x = json_value.get("deltaX").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let delta_y = json_value.get("deltaY").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;

                                    let position = Vector2::new(x, y);
                                    let button_mask = *CURRENT_BUTTON_MASK.lock().unwrap();
                                    let modifiers = (
                                        json_value.get("shift").and_then(|v| v.as_bool()).unwrap_or(false),
                                        json_value.get("ctrl").and_then(|v| v.as_bool()).unwrap_or(false),
                                        json_value.get("alt").and_then(|v| v.as_bool()).unwrap_or(false),
                                        json_value.get("meta").and_then(|v| v.as_bool()).unwrap_or(false),
                                    );

                                    if delta_y != 0.0 {
                                        let button = if delta_y < 0.0 { MouseButton::WHEEL_UP } else { MouseButton::WHEEL_DOWN };
                                        let factor = (delta_y.abs() / 100.0).max(1.0);
                                        send_wheel_event(button, position, factor, button_mask, modifiers);
                                    }

                                    if delta_x != 0.0 {
                                        let button = if delta_x < 0.0 { MouseButton::WHEEL_LEFT } else { MouseButton::WHEEL_RIGHT };
                                        let factor = (delta_x.abs() / 100.0).max(1.0);
                                        send_wheel_event(button, position, factor, button_mask, modifiers);
                                    }

                                    return;
                                },

                                "_key_down" | "_key_up" => {
                                    let key_str = json_value.get("key").and_then(|v| v.as_str()).unwrap_or("");
                                    let mut event = InputEventKey::new_gd();
                                    
                                    let godot_key = GODOT_KEYS.get(key_str).copied().unwrap_or(Key::NONE);
                                    
                                    event.set_keycode(godot_key);
                                    event.set_pressed(event_type == "_key_down");

                                    event.set_shift_pressed(json_value.get("shift").and_then(|v| v.as_bool()).unwrap_or(false));
                                    event.set_ctrl_pressed(json_value.get("ctrl").and_then(|v| v.as_bool()).unwrap_or(false));
                                    event.set_alt_pressed(json_value.get("alt").and_then(|v| v.as_bool()).unwrap_or(false));
                                    event.set_meta_pressed(json_value.get("meta").and_then(|v| v.as_bool()).unwrap_or(false));
                                    
                                    Input::singleton().parse_input_event(&event);
                                    return;
                                },
                                
                                _ => {}
                            }
                        }
                    }
                    
                    // if we get here, this is a regular IPC message
                    base.emit_signal("ipc_message", &[body.to_variant()]);
                }
            })
            .with_on_page_load_handler({
                let base = Arc::clone(&base);
                move | event: PageLoadEvent, url: String | {
                    let mut base = base.lock().unwrap();

                    match event {
                        PageLoadEvent::Started => base.emit_signal("page_load_started", &[url.to_variant()]),
                        PageLoadEvent::Finished => base.emit_signal("page_load_finished", &[url.to_variant()]),
                    };
                }
            })
            .with_custom_protocol(
                "res".into(), move |_webview_id, request| get_res_response(request),
            );

        if !self.url.is_empty() && !self.html.is_empty() {
            godot_error!("[Godot WRY] You have entered both a URL and HTML code. You may only enter one at a time.")
        }

        let webview = webview_builder.build_as_child(&window).unwrap();
        self.webview.replace(webview);

        let mut viewport = self.base().get_tree().expect("Could not get tree").get_root().expect("Could not get viewport");
        viewport.connect("size_changed", &Callable::from_object_method(&*self.base(), "resize"));

        self.base().clone().connect("resized", &Callable::from_object_method(&*self.base(), "resize"));
        self.base().clone().connect("visibility_changed", &Callable::from_object_method(&*self.base(), "update_visibility"));

        if self.forward_input_events {
            let forward_script = r#"
                document.addEventListener('mousemove', (e) => {
                    if (!document.hasFocus()) return;
                    window.ipc.postMessage(JSON.stringify({
                        type: '_mouse_move',
                        x: e.clientX * window.devicePixelRatio,
                        y: e.clientY * window.devicePixelRatio,
                        movementX: e.movementX * window.devicePixelRatio,
                        movementY: e.movementY * window.devicePixelRatio,
                        button: e.button
                    }));
                });
                document.addEventListener('mousedown', (e) => {
                    if (!document.hasFocus()) return;
                    window.ipc.postMessage(JSON.stringify({
                        type: '_mouse_down',
                        x: e.clientX * window.devicePixelRatio,
                        y: e.clientY * window.devicePixelRatio,
                        button: e.button
                    }));
                });
                document.addEventListener('mouseup', (e) => {
                    if (!document.hasFocus()) return;
                    window.ipc.postMessage(JSON.stringify({
                        type: '_mouse_up', 
                        x: e.clientX * window.devicePixelRatio,
                        y: e.clientY * window.devicePixelRatio,
                        button: e.button
                    }));
                });
                document.addEventListener('wheel', (e) => {
                    if (!document.hasFocus()) return;
                    window.ipc.postMessage(JSON.stringify({
                        type: '_mouse_wheel',
                        x: e.clientX * window.devicePixelRatio,
                        y: e.clientY * window.devicePixelRatio,
                        deltaX: e.deltaX,
                        deltaY: e.deltaY,
                        shift: e.shiftKey,
                        ctrl: e.ctrlKey,
                        alt: e.altKey,
                        meta: e.metaKey
                    }));
                });
                document.addEventListener('keydown', (e) => {
                    if (!document.hasFocus()) return;
                    const isModifier = ["Alt", "Shift", "Control", "Meta"].includes(e.key);
                    window.ipc.postMessage(JSON.stringify({
                        type: '_key_down',
                        key: e.key,
                        code: e.code,
                        keyCode: e.keyCode,
                        shift: isModifier ? false : e.shiftKey,
                        ctrl: isModifier ? false : e.ctrlKey,
                        alt: isModifier ? false : e.altKey,
                        meta: isModifier ? false : e.metaKey
                    }));
                });
                document.addEventListener('keyup', (e) => {
                    if (!document.hasFocus()) return;
                    const isModifier = ["Alt", "Shift", "Control", "Meta"].includes(e.key);
                    window.ipc.postMessage(JSON.stringify({
                        type: '_key_up',
                        key: e.key,
                        code: e.code,
                        keyCode: e.keyCode,
                        shift: isModifier ? false : e.shiftKey,
                        ctrl: isModifier ? false : e.ctrlKey,
                        alt: isModifier ? false : e.altKey,
                        meta: isModifier ? false : e.metaKey
                    }));
                });
            "#;
            
            if let Some(ref webview) = self.webview {
                let _ = webview.evaluate_script(forward_script);
            }
        }

        self.resize()
    }

    #[func]
    fn post_message(&self, message: GString) {
        if let Some(webview) = &self.webview {
            let data = serde_json::json!({ "detail": String::from(message) });
            let script = format!("document.dispatchEvent(new CustomEvent('message', {}))", data);
            let _ = webview.evaluate_script(&script);
        }
    }

    #[func]
    fn resize(&self) {
        let (x, y, w, h) = if self.full_window_size {
            let viewport_size = self.base().get_tree().expect("Could not get tree").get_root().expect("Could not get viewport").get_size();
            (0, 0, viewport_size.x, viewport_size.y)
        } else {
            let pos = self.base().get_screen_position();
            let size = self.base().get_size();
            (pos.x as i32, pos.y as i32, size.x as i32, size.y as i32)
        };

        #[cfg(target_os = "android")]
        {
            // Godot の get_screen_position/get_size は content-scale 済みの論理座標。
            // Android DecorView はデバイスピクセル。
            // window_get_size (物理) / visible_rect (論理) でスケール算出
            let ds = DisplayServer::singleton();
            let win_size = ds.window_get_size();
            let vp_size = self.base().get_viewport().expect("viewport")
                .get_visible_rect().size;
            let scale_x = win_size.x as f32 / vp_size.x as f32;
            let scale_y = win_size.y as f32 / vp_size.y as f32;

            let px = (x as f32 * scale_x) as i32;
            let py = (y as f32 * scale_y) as i32;
            let pw = (w as f32 * scale_x) as i32;
            let ph = (h as f32 * scale_y) as i32;
            godot_print!("[Godot WRY] resize: godot=({x},{y},{w},{h}) win=({},{}) vp=({},{}) scale=({scale_x},{scale_y}) px=({px},{py},{pw},{ph})",
                win_size.x, win_size.y, vp_size.x, vp_size.y);

            use jni::objects::JValue;
            android_bridge_call("setBounds", "(IIII)V", &[
                JValue::Int(px), JValue::Int(py), JValue::Int(pw), JValue::Int(ph),
            ]);
            return;
        }

        #[cfg(not(target_os = "android"))]
        if let Some(webview) = &self.webview {
            let rect = Rect {
                position: PhysicalPosition::new(x, y).into(),
                size: PhysicalSize::new(w, h).into(),
            };
            let _ = webview.set_bounds(rect);
        }
    }

    /// GDScript から直接デバイスピクセル座標で WebView 位置を指定（Android 用）
    #[func]
    fn set_bounds_device_px(&self, x: i32, y: i32, w: i32, h: i32) {
        #[cfg(target_os = "android")]
        {
            use jni::objects::JValue;
            android_bridge_call("setBounds", "(IIII)V", &[
                JValue::Int(x), JValue::Int(y), JValue::Int(w), JValue::Int(h),
            ]);
            return;
        }

        #[cfg(not(target_os = "android"))]
        if let Some(webview) = &self.webview {
            let rect = Rect {
                position: PhysicalPosition::new(x, y).into(),
                size: PhysicalSize::new(w, h).into(),
            };
            let _ = webview.set_bounds(rect);
        }
    }

    /// Android WebView の下部角丸を設定（デバイスピクセル単位）
    #[func]
    fn set_corner_radius(&self, radius: f32) {
        #[cfg(target_os = "android")]
        {
            use jni::objects::JValue;
            android_bridge_call("setCornerRadius", "(F)V", &[JValue::Float(radius)]);
        }
    }

    #[func]
    fn eval(&self, script: GString) {
        if let Some(webview) = &self.webview {
            let _ = webview.evaluate_script(&*String::from(script));
        }
    }

    #[func]
    fn update_visibility(&self) {
        if let Some(webview) = &self.webview {
            let visibility = self.base().is_visible_in_tree();
            webview.set_visible(visibility).expect("Could not set visibility");
            self.resize()
        }
    }

    #[func]
    fn set_visible(&self, visibility: bool) {
        #[cfg(target_os = "android")]
        {
            use jni::objects::JValue;
            android_bridge_call("setVisible", "(Z)V", &[JValue::Bool(visibility as u8)]);
            return;
        }

        #[cfg(not(target_os = "android"))]
        if let Some(webview) = &self.webview {
            let _ = webview.set_visible(visibility);
        }
    }

    #[func]
    fn load_html(&self, html: GString) {
        if let Some(webview) = &self.webview {
            let _ = webview.load_html(&*String::from(html));
        }
    }

    #[func]
    fn load_url(&self, url: GString) {
        let mut url_str = String::from(url);

        if let Some(stripped) = url_str.strip_prefix("res://") {
            let path = stripped.replace("\\", "/");
            url_str = format!("http://res.{}", path);
        }

        #[cfg(target_os = "android")]
        {
            if let Ok(guard) = ANDROID_BRIDGE.lock() {
                if let Some(state) = guard.as_ref() {
                    if let Ok(mut env) = state.vm.attach_current_thread() {
                        let cls = unsafe { jni::objects::JClass::from_raw(state.bridge_class.as_raw()) };
                        if let Ok(jurl) = env.new_string(&url_str) {
                            let _ = env.call_static_method(cls, "loadUrl",
                                "(Ljava/lang/String;)V", &[(&jurl).into()]);
                        }
                    }
                }
            }
            return;
        }

        #[cfg(not(target_os = "android"))]
        if let Some(webview) = &self.webview {
            let _ = webview.load_url(&url_str);
        }
    }

    #[func]
    fn clear_all_browsing_data(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.clear_all_browsing_data();
        }
    }

    #[func]
    fn close_devtools(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.close_devtools();
        }
    }

    #[func]
    fn open_devtools(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.open_devtools();
        }
    }

    #[func]
    fn is_devtools_open(&self) -> bool {
        if let Some(webview) = &self.webview {
            return webview.is_devtools_open();
        }
        false
    }

    #[func]
    fn focus(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.focus();
        }
    }

    #[func]
    fn focus_parent(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.focus_parent();
        }
    }

    #[func]
    fn print(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.print();
        }
    }

    #[func]
    fn reload(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.reload();
        }
    }

    #[func]
    fn zoom(&self, scale_factor: f64) {
        if let Some(webview) = &self.webview {
            let _ = webview.zoom(scale_factor);
        }
    }
}

#[cfg(target_os = "android")]
static WRY_BRIDGE_DEX: &[u8] = include_bytes!("wry_bridge.dex");

#[cfg(target_os = "android")]
static ANDROID_BRIDGE: std::sync::Mutex<Option<AndroidBridgeState>> = std::sync::Mutex::new(None);

#[cfg(target_os = "android")]
struct AndroidBridgeState {
    vm: jni::JavaVM,
    bridge_class: jni::objects::GlobalRef,
}

#[cfg(target_os = "android")]
unsafe impl Send for AndroidBridgeState {}
#[cfg(target_os = "android")]
unsafe impl Sync for AndroidBridgeState {}

#[cfg(target_os = "android")]
fn android_bridge_call(method: &str, sig: &str, args: &[jni::objects::JValue]) {
    if let Ok(guard) = ANDROID_BRIDGE.lock() {
        if let Some(state) = guard.as_ref() {
            if let Ok(mut env) = state.vm.attach_current_thread() {
                let cls = unsafe { jni::objects::JClass::from_raw(state.bridge_class.as_raw()) };
                if let Err(e) = env.call_static_method(cls, method, sig, args) {
                    godot_error!("[Godot WRY] Android bridge call {method} failed: {e}");
                }
            }
        }
    }
}

/// Android: JVM + Activity を取得するヘルパー
#[cfg(target_os = "android")]
fn get_android_jvm_and_activity() -> Result<(jni::JavaVM, jni::objects::GlobalRef), String> {
    // Get JVM via dlsym
    let jvm_ptr = unsafe {
        type JniGetCreatedJavaVMsFn = unsafe extern "C" fn(
            *mut *mut jni::sys::JavaVM, jni::sys::jsize, *mut jni::sys::jsize,
        ) -> jni::sys::jint;

        let libs = [
            b"libnativehelper.so\0".as_ptr() as *const _,
            b"libart.so\0".as_ptr() as *const _,
        ];
        let sym_name = b"JNI_GetCreatedJavaVMs\0".as_ptr() as *const _;
        let mut func_ptr: *mut std::ffi::c_void = libc::dlsym(libc::RTLD_DEFAULT, sym_name);

        if func_ptr.is_null() {
            for lib in &libs {
                let handle = libc::dlopen(*lib, libc::RTLD_LAZY | libc::RTLD_NOLOAD);
                if !handle.is_null() {
                    func_ptr = libc::dlsym(handle, sym_name);
                    libc::dlclose(handle);
                    if !func_ptr.is_null() { break; }
                }
                let handle = libc::dlopen(*lib, libc::RTLD_LAZY);
                if !handle.is_null() {
                    func_ptr = libc::dlsym(handle, sym_name);
                    if !func_ptr.is_null() { break; }
                    libc::dlclose(handle);
                }
            }
        }
        if func_ptr.is_null() {
            return Err("Could not find JNI_GetCreatedJavaVMs".into());
        }
        let func: JniGetCreatedJavaVMsFn = std::mem::transmute(func_ptr);
        let mut ptr: *mut jni::sys::JavaVM = std::ptr::null_mut();
        let mut count: jni::sys::jsize = 0;
        func(&mut ptr, 1, &mut count);
        if count == 0 || ptr.is_null() {
            return Err("No JavaVM found".into());
        }
        ptr
    };

    let vm = unsafe { jni::JavaVM::from_raw(jvm_ptr) }
        .map_err(|e| format!("Failed to wrap JavaVM: {e}"))?;

    let global_activity = {
        let mut env = vm.attach_current_thread()
            .map_err(|e| format!("Failed to attach thread: {e}"))?;

        // Get Activity via ActivityThread reflection
        let at_class = env.find_class("android/app/ActivityThread")
            .map_err(|e| format!("Failed to find ActivityThread: {e}"))?;
        let at_obj = env.call_static_method(&at_class, "currentActivityThread",
            "()Landroid/app/ActivityThread;", &[])
            .map_err(|e| format!("currentActivityThread failed: {e}"))?.l()
            .map_err(|e| format!("Failed to get AT object: {e}"))?;
        let activities = env.get_field(&at_obj, "mActivities", "Landroid/util/ArrayMap;")
            .map_err(|e| format!("Failed to get mActivities: {e}"))?.l()
            .map_err(|e| format!("Failed to unwrap mActivities: {e}"))?;
        let values = env.call_method(&activities, "values", "()Ljava/util/Collection;", &[])
            .map_err(|e| format!("values() failed: {e}"))?.l()
            .map_err(|e| format!("Failed to get values: {e}"))?;
        let array = env.call_method(&values, "toArray", "()[Ljava/lang/Object;", &[])
            .map_err(|e| format!("toArray() failed: {e}"))?.l()
            .map_err(|e| format!("Failed to get array: {e}"))?;
        let arr = jni::objects::JObjectArray::from(array);
        let arr_len = env.get_array_length(&arr).map_err(|e| format!("array length: {e}"))?;

        let mut activity = jni::objects::JObject::null();
        for i in 0..arr_len {
            let record = env.get_object_array_element(&arr, i)
                .map_err(|e| format!("get record[{i}]: {e}"))?;
            let act = env.get_field(&record, "activity", "Landroid/app/Activity;")
                .map_err(|e| format!("get activity field: {e}"))?.l()
                .map_err(|e| format!("unwrap activity: {e}"))?;
            if !act.is_null() { activity = act; break; }
        }
        if activity.is_null() {
            return Err("No running Activity found".into());
        }

        env.new_global_ref(&activity)
            .map_err(|e| format!("global ref: {e}"))?
    }; // env dropped here, vm borrow released

    Ok((vm, global_activity))
}

/// Android: DEX ブリッジ経由で WebView を作成
#[cfg(target_os = "android")]
fn create_android_webview(url: &str) -> Result<(), String> {
    let (vm, activity_ref) = get_android_jvm_and_activity()?;

    let global_class = {
        let mut env = vm.attach_current_thread()
            .map_err(|e| format!("Failed to attach thread: {e}"))?;

        // DEX をメモリから読み込み (InMemoryDexClassLoader, API 26+)
        let dex_buf = unsafe {
            env.new_direct_byte_buffer(
                std::mem::transmute::<*const u8, *mut u8>(WRY_BRIDGE_DEX.as_ptr()),
                WRY_BRIDGE_DEX.len(),
            )
        }.map_err(|e| format!("Failed to create ByteBuffer: {e}"))?;

        let parent_cl_class = env.find_class("java/lang/ClassLoader")
            .map_err(|e| format!("ClassLoader not found: {e}"))?;
        let parent_cl = env.call_static_method(&parent_cl_class, "getSystemClassLoader",
            "()Ljava/lang/ClassLoader;", &[])
            .map_err(|e| format!("getSystemClassLoader failed: {e}"))?.l()
            .map_err(|e| format!("Failed to get ClassLoader: {e}"))?;

        let dex_cl_class = env.find_class("dalvik/system/InMemoryDexClassLoader")
            .map_err(|e| format!("InMemoryDexClassLoader not found: {e}"))?;
        let dex_cl = env.new_object(&dex_cl_class,
            "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
            &[(&dex_buf).into(), (&parent_cl).into()])
            .map_err(|e| format!("Failed to create DexClassLoader: {e}"))?;

        // WryBridge クラスをロード
        let bridge_name = env.new_string("org.nicetry.wry.WryBridge")
            .map_err(|e| format!("Failed to create string: {e}"))?;
        let bridge_class = env.call_method(&dex_cl, "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[(&bridge_name).into()])
            .map_err(|e| format!("Failed to load WryBridge class: {e}"))?.l()
            .map_err(|e| format!("Failed to unwrap class: {e}"))?;

        let bridge_class_ref = jni::objects::JClass::from(bridge_class);

        // WryBridge.init(activity)
        env.call_static_method(&bridge_class_ref, "init",
            "(Landroid/app/Activity;)V",
            &[(&*activity_ref).into()])
            .map_err(|e| format!("WryBridge.init() failed: {e}"))?;

        // WryBridge.createWebView(url) — ブロッキング（UI スレッドで実行）
        let url_str = env.new_string(url)
            .map_err(|e| format!("Failed to create URL string: {e}"))?;
        env.call_static_method(&bridge_class_ref, "createWebView",
            "(Ljava/lang/String;)V",
            &[(&url_str).into()])
            .map_err(|e| format!("WryBridge.createWebView() failed: {e}"))?;

        env.new_global_ref(&bridge_class_ref)
            .map_err(|e| format!("Failed to create global ref: {e}"))?
    }; // env dropped, vm borrow released

    *ANDROID_BRIDGE.lock().unwrap() = Some(AndroidBridgeState { vm, bridge_class: global_class });
    godot_print!("[Godot WRY] Android WebView created via DEX bridge");
    Ok(())
}

// Legacy: ndk-context 初期化（WRY 用、Android では現在使用しない）
#[cfg(target_os = "android")]
fn initialize_android_context() -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INITIALIZED: AtomicBool = AtomicBool::new(false);

    if INITIALIZED.load(Ordering::Relaxed) {
        return Ok(());
    }

    // 1. Get the JavaVM from the running process via dlsym
    //    JNI_GetCreatedJavaVMs lives in libnativehelper.so or libart.so (not in NDK sysroot)
    let jvm_ptr = unsafe {
        type JniGetCreatedJavaVMsFn = unsafe extern "C" fn(
            *mut *mut jni::sys::JavaVM,
            jni::sys::jsize,
            *mut jni::sys::jsize,
        ) -> jni::sys::jint;

        // Try multiple library locations where JNI_GetCreatedJavaVMs might live
        let libs = [
            b"libnativehelper.so\0".as_ptr() as *const _,
            b"libart.so\0".as_ptr() as *const _,
            b"libnativebridge.so\0".as_ptr() as *const _,
        ];

        let mut func_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let sym_name = b"JNI_GetCreatedJavaVMs\0".as_ptr() as *const _;

        // First try RTLD_DEFAULT (works if already loaded)
        func_ptr = libc::dlsym(libc::RTLD_DEFAULT, sym_name);

        // If not found, try loading specific libraries
        if func_ptr.is_null() {
            for lib in &libs {
                let handle = libc::dlopen(*lib, libc::RTLD_LAZY | libc::RTLD_NOLOAD);
                if !handle.is_null() {
                    func_ptr = libc::dlsym(handle, sym_name);
                    libc::dlclose(handle);
                    if !func_ptr.is_null() {
                        break;
                    }
                }
                // Try loading it fresh
                let handle = libc::dlopen(*lib, libc::RTLD_LAZY);
                if !handle.is_null() {
                    func_ptr = libc::dlsym(handle, sym_name);
                    // Don't close - keep it loaded
                    if !func_ptr.is_null() {
                        break;
                    }
                    libc::dlclose(handle);
                }
            }
        }

        if func_ptr.is_null() {
            return Err("Could not find JNI_GetCreatedJavaVMs in any library".into());
        }

        let func: JniGetCreatedJavaVMsFn = std::mem::transmute(func_ptr);

        let mut jvm_ptr: *mut jni::sys::JavaVM = std::ptr::null_mut();
        let mut count: jni::sys::jsize = 0;
        func(&mut jvm_ptr, 1, &mut count);

        if count == 0 || jvm_ptr.is_null() {
            return Err("JNI_GetCreatedJavaVMs returned no VMs".into());
        }
        jvm_ptr
    };

    let vm = unsafe { jni::JavaVM::from_raw(jvm_ptr) }
        .map_err(|e| format!("Failed to wrap JavaVM: {e}"))?;
    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach to JNI thread: {e}"))?;

    // 2. Get the running Activity through ActivityThread reflection
    //    ActivityThread.currentActivityThread().mActivities → first Activity
    let at_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread class: {e}"))?;

    let at_obj = env.call_static_method(
        &at_class,
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
        &[],
    ).map_err(|e| format!("Failed to call currentActivityThread: {e}"))?
        .l().map_err(|e| format!("Failed to get ActivityThread object: {e}"))?;

    // mActivities is an ArrayMap<IBinder, ActivityClientRecord> on API 19+
    let activities = env.get_field(&at_obj, "mActivities", "Landroid/util/ArrayMap;")
        .map_err(|e| format!("Failed to get mActivities: {e}"))?
        .l().map_err(|e| format!("Failed to unwrap mActivities: {e}"))?;

    let values = env.call_method(&activities, "values", "()Ljava/util/Collection;", &[])
        .map_err(|e| format!("Failed to call values(): {e}"))?
        .l().map_err(|e| format!("Failed to get values collection: {e}"))?;

    let array = env.call_method(&values, "toArray", "()[Ljava/lang/Object;", &[])
        .map_err(|e| format!("Failed to call toArray(): {e}"))?
        .l().map_err(|e| format!("Failed to get array: {e}"))?;

    let arr = jni::objects::JObjectArray::from(array);
    let arr_len = env.get_array_length(&arr)
        .map_err(|e| format!("Failed to get array length: {e}"))?;

    if arr_len == 0 {
        return Err("No activities found in ActivityThread.mActivities".into());
    }

    // Find the first non-null activity from the records
    let mut activity = jni::objects::JObject::null();
    for i in 0..arr_len {
        let record = env.get_object_array_element(&arr, i)
            .map_err(|e| format!("Failed to get ActivityClientRecord[{i}]: {e}"))?;

        let act = env.get_field(&record, "activity", "Landroid/app/Activity;")
            .map_err(|e| format!("Failed to get activity field: {e}"))?
            .l().map_err(|e| format!("Failed to unwrap activity: {e}"))?;

        if !act.is_null() {
            activity = act;
            break;
        }
    }

    if activity.is_null() {
        return Err("All Activity references in mActivities are null".into());
    }

    // 3. Create a global reference (prevents GC)
    let global_ref = env.new_global_ref(&activity)
        .map_err(|e| format!("Failed to create global ref for Activity: {e}"))?;

    // 4. Initialize ndk-context so WRY can access the JVM and Activity
    unsafe {
        ndk_context::initialize_android_context(
            jvm_ptr as *mut std::ffi::c_void,
            global_ref.as_raw() as *mut std::ffi::c_void,
        );
    }

    // Leak the global ref to keep it alive for the process lifetime
    std::mem::forget(global_ref);

    INITIALIZED.store(true, Ordering::Relaxed);
    godot_print!("[Godot WRY] Android context initialized successfully");
    Ok(())
}

fn send_wheel_event(
    button: MouseButton,
    position: Vector2,
    factor: f32,
    button_mask: MouseButtonMask,
    modifiers: (bool, bool, bool, bool),
) {
    let (shift, ctrl, alt, meta) = modifiers;
    for pressed in [true, false] {
        let mut event = InputEventMouseButton::new_gd();
        event.set_button_index(button);
        event.set_position(position);
        event.set_global_position(position);
        event.set_pressed(pressed);
        event.set_factor(factor);
        event.set_button_mask(button_mask);
        event.set_shift_pressed(shift);
        event.set_ctrl_pressed(ctrl);
        event.set_alt_pressed(alt);
        event.set_meta_pressed(meta);
        Input::singleton().parse_input_event(&event);
    }
}

lazy_static! {
    static ref CURRENT_BUTTON_MASK: Mutex<MouseButtonMask> = Mutex::new(MouseButtonMask::default());

    static ref GODOT_KEYS: HashMap<&'static str, Key> = HashMap::from([
        // https://docs.godotengine.org/en/stable/classes/class_%40globalscope.html#enum-globalscope-key

        ("a", Key::A),
        ("A", Key::A),
        ("b", Key::B),
        ("B", Key::B),
        ("c", Key::C),
        ("C", Key::C),
        ("d", Key::D),
        ("D", Key::D),
        ("e", Key::E),
        ("E", Key::E),
        ("f", Key::F),
        ("F", Key::F),
        ("g", Key::G),
        ("G", Key::G),
        ("h", Key::H),
        ("H", Key::H),
        ("i", Key::I),
        ("I", Key::I),
        ("j", Key::J),
        ("J", Key::J),
        ("k", Key::K),
        ("K", Key::K),
        ("l", Key::L),
        ("L", Key::L),
        ("m", Key::M),
        ("M", Key::M),
        ("n", Key::N),
        ("N", Key::N),
        ("o", Key::O),
        ("O", Key::O),
        ("p", Key::P),
        ("P", Key::P),
        ("q", Key::Q),
        ("Q", Key::Q),
        ("r", Key::R),
        ("R", Key::R),
        ("s", Key::S),
        ("S", Key::S),
        ("t", Key::T),
        ("T", Key::T),
        ("u", Key::U),
        ("U", Key::U),
        ("v", Key::V),
        ("V", Key::V),
        ("w", Key::W),
        ("W", Key::W),
        ("x", Key::X),
        ("X", Key::X),
        ("y", Key::Y),
        ("Y", Key::Y),
        ("z", Key::Z),
        ("Z", Key::Z),
        
        ("0", Key::KEY_0),
        ("1", Key::KEY_1),
        ("2", Key::KEY_2),
        ("3", Key::KEY_3),
        ("4", Key::KEY_4),
        ("5", Key::KEY_5),
        ("6", Key::KEY_6),
        ("7", Key::KEY_7),
        ("8", Key::KEY_8),
        ("9", Key::KEY_9),
        ("Numpad0", Key::KP_0),
        ("Numpad1", Key::KP_1),
        ("Numpad2", Key::KP_2),
        ("Numpad3", Key::KP_3),
        ("Numpad4", Key::KP_4),
        ("Numpad5", Key::KP_5),
        ("Numpad6", Key::KP_6),
        ("Numpad7", Key::KP_7),
        ("Numpad8", Key::KP_8),
        ("Numpad9", Key::KP_9),
        
        ("F1", Key::F1),
        ("F2", Key::F2),
        ("F3", Key::F3),
        ("F4", Key::F4),
        ("F5", Key::F5),
        ("F6", Key::F6),
        ("F7", Key::F7),
        ("F8", Key::F8),
        ("F9", Key::F9),
        ("F10", Key::F10),
        ("F11", Key::F11),
        ("F12", Key::F12),
        ("F13", Key::F13),
        ("F14", Key::F14),
        ("F15", Key::F15),
        ("F16", Key::F16),
        ("F17", Key::F17),
        ("F18", Key::F18),
        ("F19", Key::F19),
        ("F20", Key::F20),
        ("F21", Key::F21),
        ("F22", Key::F22),
        ("F23", Key::F23),
        ("F24", Key::F24),
        
        ("ArrowUp", Key::UP),
        ("ArrowDown", Key::DOWN),
        ("ArrowLeft", Key::LEFT),
        ("ArrowRight", Key::RIGHT),
        
        ("Enter", Key::ENTER),
        ("NumpadEnter", Key::KP_ENTER),
        ("Tab", Key::TAB),
        ("Space", Key::SPACE),
        (" ", Key::SPACE),
        ("Backspace", Key::BACKSPACE),
        ("Escape", Key::ESCAPE),
        ("CapsLock", Key::CAPSLOCK),
        ("ScrollLock", Key::SCROLLLOCK),
        ("NumLock", Key::NUMLOCK),
        ("PrintScreen", Key::PRINT),
        ("Pause", Key::PAUSE),
        ("Insert", Key::INSERT),
        ("Home", Key::HOME),
        ("PageUp", Key::PAGEUP),
        ("Delete", Key::DELETE),
        ("End", Key::END),
        ("PageDown", Key::PAGEDOWN),
        
        ("Shift", Key::SHIFT),
        ("Control", Key::CTRL),
        ("Alt", Key::ALT),
        ("AltGraph", Key::ALT),
        ("Meta", Key::META),
        ("ContextMenu", Key::MENU),
        
        ("NumpadMultiply", Key::KP_MULTIPLY),
        ("NumpadDivide", Key::KP_DIVIDE),
        ("NumpadAdd", Key::KP_ADD),
        ("NumpadSubtract", Key::KP_SUBTRACT),
        ("NumpadDecimal", Key::KP_PERIOD),
        
        ("MediaPlayPause", Key::MEDIAPLAY),
        ("MediaStop", Key::MEDIASTOP),
        ("MediaTrackNext", Key::MEDIANEXT),
        ("MediaTrackPrevious", Key::MEDIAPREVIOUS),
        ("VolumeDown", Key::VOLUMEDOWN),
        ("VolumeUp", Key::VOLUMEUP),
        ("VolumeMute", Key::VOLUMEMUTE),
        
        ("BrowserBack", Key::BACK),
        ("BrowserForward", Key::FORWARD),
        ("BrowserRefresh", Key::REFRESH),
        ("BrowserStop", Key::STOP),
        ("BrowserSearch", Key::SEARCH),
        ("BrowserHome", Key::HOMEPAGE),
        
        ("`", Key::QUOTELEFT),
        ("~", Key::ASCIITILDE),
        ("!", Key::EXCLAM),
        ("@", Key::AT),
        ("#", Key::NUMBERSIGN),
        ("$", Key::DOLLAR),
        ("%", Key::PERCENT),
        ("^", Key::ASCIICIRCUM),
        ("&", Key::AMPERSAND),
        ("*", Key::ASTERISK),
        ("(", Key::PARENLEFT),
        (")", Key::PARENRIGHT),
        ("-", Key::MINUS),
        ("_", Key::UNDERSCORE),
        ("=", Key::EQUAL),
        ("+", Key::PLUS),
        ("[", Key::BRACKETLEFT),
        ("{", Key::BRACELEFT),
        ("]", Key::BRACKETRIGHT),
        ("}", Key::BRACERIGHT),
        ("\\", Key::BACKSLASH),
        ("|", Key::BAR),
        (";", Key::SEMICOLON),
        (":", Key::COLON),
        ("'", Key::APOSTROPHE),
        ("\"", Key::QUOTEDBL),
        (",", Key::COMMA),
        ("<", Key::LESS),
        (".", Key::PERIOD),
        (">", Key::GREATER),
        ("/", Key::SLASH),
        ("?", Key::QUESTION),
    ]);
}
