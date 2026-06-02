//! The browser-engine seam (`crawler-engine-trait`).
//!
//! The crawler talks to the page through one trait so the engine is swappable
//! and the heavyweight CEF impl stays optional — the same discipline as
//! `WasmEngine` (`plugins.md`) and `egui-blitz`'s `ResourceProvider`. The
//! default build wires [`NullEngine`] (no JS, no rendering) so the shell runs
//! and the emitters/picker are exercisable with no browser in the graph; the
//! `cef` feature wires the real engine (`crawler-cef-engine`). Keeping the
//! trait also leaves room for a lighter CDP-driven external-Chromium back end.

/// A hit-test result handed back to the element picker (`crawler-element-picker`).
#[derive(Debug, Clone, Default)]
pub struct Hit {
    /// Ranked selector candidates for the hit node, most-stable first
    /// (`#id` → unique class → attribute → bounded `nth-child` path).
    pub selectors: Vec<String>,
    /// The hit node's text content (for labelling the picked field).
    pub text: String,
    /// Whether the chosen top selector matches more than one node on the page
    /// (`document.querySelectorAll(top).length > 1`) — the list/repeat signal
    /// that drives hub/list crawl mode + `next_urls` (`crawler-element-picker`).
    pub repeat: bool,
}

/// The live-page engine. Implemented by [`NullEngine`] (default) and, under the
/// `cef` feature, a CEF-backed engine.
pub trait BrowserEngine {
    /// Begin loading `url`. Navigation is async; progress surfaces via
    /// [`poll`](BrowserEngine::poll) + [`current_url`](BrowserEngine::current_url).
    fn load(&mut self, url: &str);

    /// Per-frame work for *this* browser (no-op for [`NullEngine`]). For the
    /// CEF browser this issues the off-screen `send_external_begin_frame` so a
    /// new `OnPaint` fires, and auto-refreshes the cached rendered HTML on a
    /// load→idle transition. The *global* CEF message loop is pumped once by
    /// [`cef_impl::CefRuntime::pump`], NOT here — only the active/visible tab's
    /// browser should be `poll`ed each frame so background tabs stay paused.
    fn poll(&mut self);

    /// The URL currently loaded, if any.
    fn current_url(&self) -> Option<&str>;

    /// The latest *rendered* DOM (post-JS) as an HTML string — the input the
    /// shared `hiker-extract` pipeline consumes for previews/archives
    /// (`crawler-preview-fidelity`). `None` when no engine can render JS or when
    /// no snapshot has come back yet. The result is async: call
    /// [`request_render_html`](BrowserEngine::request_render_html) to kick a
    /// refresh, then read the cached value once it lands (the engine also
    /// refreshes automatically on load-end).
    fn rendered_html(&self) -> Option<String>;

    /// Kick an async refresh of the cached rendered HTML (a `Runtime.evaluate`
    /// of `document.documentElement.outerHTML` over DevTools). No-op for
    /// [`NullEngine`]. The freshly captured HTML surfaces via
    /// [`rendered_html`](BrowserEngine::rendered_html) a few frames later.
    fn request_render_html(&mut self);

    /// Drain the most recent picker [`Hit`], if one has come back since the
    /// last call. The engine fires the hit-test as engine-specific input (CEF
    /// does so via `CefBrowser::request_pick`); because DevTools results are
    /// async the [`Hit`] surfaces here a few frames later. `None` until one
    /// lands (or always, for [`NullEngine`], which never produces hits).
    fn take_hit(&mut self) -> Option<Hit>;

    /// Capture the rendered page as WARC bytes for the direct handoff
    /// (`crawler-direct-warc` / `crawler-warc-archive`). `None` when
    /// unsupported by the active engine.
    fn capture_warc(&self) -> Option<Vec<u8>>;
}

/// The no-engine placeholder: holds the requested URL but renders nothing and
/// executes no JS. Lets the shell build and run with no browser dependency.
/// Only the default (no-`cef`) build instantiates it — under `cef` every tab is
/// a real browser — so it's gated to keep the cef binary free of dead code.
#[cfg(not(feature = "cef"))]
#[derive(Debug, Default)]
pub struct NullEngine {
    url: Option<String>,
}

#[cfg(not(feature = "cef"))]
impl NullEngine {
    /// A fresh, empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(not(feature = "cef"))]
impl BrowserEngine for NullEngine {
    fn load(&mut self, url: &str) {
        tracing::info!(url, "NullEngine: load requested (no engine compiled in)");
        self.url = Some(url.to_owned());
    }

    fn poll(&mut self) {}

    fn current_url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    fn rendered_html(&self) -> Option<String> {
        None
    }

    fn request_render_html(&mut self) {}

    fn take_hit(&mut self) -> Option<Hit> {
        None
    }

    fn capture_warc(&self) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(all(test, not(feature = "cef")))]
mod tests {
    use super::{BrowserEngine, Hit, NullEngine};

    #[test]
    fn hit_carries_ranked_selectors_and_the_repeat_flag() {
        let hit = Hit {
            selectors: vec!["#main".to_owned(), ".article".to_owned()],
            text: "hi".to_owned(),
            repeat: true,
        };
        assert_eq!(hit.selectors.first().map(String::as_str), Some("#main"));
        assert!(hit.repeat);
        assert_eq!(hit.text, "hi");
    }

    #[test]
    fn null_engine_tracks_the_url_but_renders_nothing() {
        let mut engine = NullEngine::new();
        assert_eq!(engine.current_url(), None);
        engine.load("https://example.com/article");
        engine.poll();
        assert_eq!(engine.current_url(), Some("https://example.com/article"));
        // No engine → no rendered DOM, no picks, no archive. The async
        // trait methods are still callable; exercising them here covers the
        // default build's engine API without a live browser.
        engine.request_render_html();
        assert!(engine.rendered_html().is_none());
        assert!(engine.take_hit().is_none());
        assert!(engine.capture_warc().is_none());
    }
}

/// The CEF-backed engine (`crawler-cef-engine`), split into a process-global
/// [`cef_impl::CefRuntime`] (CEF init + the message-loop pump) and a per-tab
/// [`cef_impl::CefBrowser`] (one windowless OSR browser). Each browser's
/// `OnPaint` BGRA buffer is uploaded as an egui texture (the same
/// CPU-buffer-to-texture pattern `egui-blitz` uses, no GL sharing).
/// `rendered_html` and the picker (`request_pick`/`take_hit`) run JS in the
/// page via CEF's DevTools protocol (`Runtime.evaluate` of
/// `document.documentElement.outerHTML`, `document.elementFromPoint` + selector
/// ranking), with results routed back through a DevTools message observer.
///
/// `capture_warc` taps CEF's CDP `Network` responses (`Network.responseReceived`
/// metadata + `Network.getResponseBody` bodies, accumulated per navigation) and
/// assembles them into a WARC byte stream via [`crate::warc::assemble`]
/// (`crawler-warc-archive`). Bodies whose async round-trip lands after eviction
/// are skipped — see the timing TODO on [`CefBrowser::drain_body_fetches`].
///
/// The CEF wiring is fully quarantined in the public [`cef_impl`] module
/// (referenced as `engine::cef_impl::CefBrowser` etc.) rather than re-exported,
/// keeping the seam explicit. Mirrors the `references/cef-rs/examples/osr` CEF
/// setup (windowless browser, render handler, external begin-frame,
/// do-message-loop-work pump) but replaces its wgpu/webrender path with a plain
/// CPU hand-off: each `OnPaint` BGRA buffer is copied into shared state for the
/// app to upload as an egui texture (the same pattern `egui-blitz` uses).
///
/// CEF init is process-global, so it lives on [`cef_impl::CefRuntime`] and runs
/// exactly once; per-tab browsers are created against the live runtime.
#[cfg(feature = "cef")]
pub mod cef_impl {
    use std::sync::{Arc, Mutex};

    // The `wrap_client!` / `wrap_render_handler!` macros expand to bare
    // references to these CEF types/traits, so each must be imported by name
    // (rather than a wildcard) into the module the macro is invoked in.
    // The `wrap_client!` / `wrap_render_handler!` macros expand to bare
    // references to these CEF types/traits, so each must be imported by name
    // (rather than a wildcard) into the module the macro is invoked in. The
    // `Impl*` traits carry the inherent browser/host/frame methods, and
    // `rc::Rc` carries `add_ref` that the wrappers rely on.
    use cef::{
        Browser, BrowserSettings, CefString, Client, DevToolsMessageObserver, ImplBrowser,
        ImplBrowserHost, ImplClient, ImplDevToolsMessageObserver, ImplDictionaryValue, ImplFrame,
        ImplRenderHandler, KeyEvent, KeyEventType, MouseButtonType, MouseEvent, Registration,
        RenderHandler, Settings, WindowInfo, WrapClient, WrapDevToolsMessageObserver,
        WrapRenderHandler, api_hash, args::Args, browser_host_create_browser_sync,
        dictionary_value_create, do_message_loop_work, execute_process, initialize, rc::Rc, sys,
        wrap_client, wrap_dev_tools_message_observer, wrap_render_handler,
    };

    use super::{BrowserEngine, Hit};

    /// The latest off-screen frame: a tightly-packed BGRA buffer plus its pixel
    /// dimensions. CEF writes this from `OnPaint`; the app drains it once per
    /// frame to (re)build the egui texture.
    #[derive(Default)]
    struct Frame {
        bgra: Vec<u8>,
        width: i32,
        height: i32,
        /// Set on each paint, cleared when the app takes the frame, so the
        /// texture is only rebuilt when the page actually changed.
        dirty: bool,
    }

    /// The view size CEF asks for in `view_rect` (logical/DIP px) plus the
    /// HiDPI scale CEF reports through `screen_info`. The app keeps both in
    /// sync with the central panel so the page reflows to fit and renders into
    /// a physical-pixel backing buffer (crisp on HiDPI).
    #[derive(Clone, Copy)]
    struct ViewSize {
        width: i32,
        height: i32,
        /// `device_scale_factor` to report to CEF — the egui `pixels_per_point`.
        /// CEF multiplies `view_rect` by this for the backing buffer, so
        /// `on_paint` delivers a buffer at physical-pixel dimensions.
        scale: f32,
    }

    impl Default for ViewSize {
        fn default() -> Self {
            // Non-zero: CEF rejects a zero-sized OSR view.
            Self {
                width: 1280,
                height: 800,
                scale: 1.0,
            }
        }
    }

    /// The DevTools request id for the rendered-HTML `Runtime.evaluate`. Fixed
    /// (we only keep the latest snapshot), so results are routed by id without
    /// a per-call counter.
    const REQ_RENDER_HTML: i32 = 1;
    /// The DevTools request id for the picker `Runtime.evaluate`.
    const REQ_PICK: i32 = 2;
    /// The DevTools request id used to enable the `Network` domain at browser
    /// creation (`crawler-warc-archive`). Its result carries nothing we route.
    const REQ_NETWORK_ENABLE: i32 = 3;
    /// Base for `Network.getResponseBody` request ids: each in-flight body
    /// fetch uses `REQ_GET_BODY_BASE + <slot>`, and the result is routed back to
    /// the captured response at that slot. Kept well clear of the fixed ids
    /// above so routing never collides.
    const REQ_GET_BODY_BASE: i32 = 1000;

    /// DevTools result slots: the latest rendered-HTML snapshot and the latest
    /// picker hit, each filled by the message observer keyed on its request id.
    #[derive(Default)]
    struct Results {
        html: Option<String>,
        hit: Option<Hit>,
    }

    /// Accumulated CDP `Network` capture for the current navigation
    /// (`crawler-warc-archive`). Metadata lands from `Network.responseReceived`
    /// keyed by CDP `requestId`; bodies arrive later via a
    /// `Network.getResponseBody` round-trip and are matched back through
    /// [`Self::pending_bodies`]. Reset on each navigation so the archive only
    /// covers the page in view.
    #[derive(Default)]
    struct Capture {
        /// The URL of the navigation this capture covers (the WARC's page URL).
        page_url: String,
        /// Responses in arrival order; each carries the metadata and (once the
        /// body round-trip lands) the body bytes.
        responses: Vec<crate::warc::CapturedResponse>,
        /// CDP `requestId` → index into [`Self::responses`], so a later
        /// `responseReceived`/`getResponseBody` can find its entry.
        by_request: std::collections::HashMap<String, usize>,
        /// Request ids whose load finished and whose body still needs fetching.
        /// The observer can't issue CDP methods (it has no host), so it queues
        /// the id here and the browser's `poll` drains it, issuing
        /// `Network.getResponseBody` with a host it does hold.
        bodies_to_fetch: Vec<String>,
        /// `getResponseBody` message id (`REQ_GET_BODY_BASE + slot`) → index
        /// into [`Self::responses`], so the async body result routes back to
        /// the right entry.
        pending_bodies: std::collections::HashMap<i32, usize>,
        /// Monotonic slot counter for the next `getResponseBody` message id.
        next_body_slot: i32,
    }

    impl Capture {
        /// Drop everything for a fresh navigation to `page_url`.
        fn reset(&mut self, page_url: &str) {
            self.page_url = page_url.to_owned();
            self.responses.clear();
            self.by_request.clear();
            self.bodies_to_fetch.clear();
            self.pending_bodies.clear();
            self.next_body_slot = 0;
        }
    }

    /// Shared state the render handler / DevTools observer write and the
    /// engine/app read. Cloning shares the same `Arc`s (the `wrap_*!` macros
    /// require `Clone` fields).
    #[derive(Clone, Default)]
    struct Shared {
        frame: Arc<Mutex<Frame>>,
        size: Arc<Mutex<ViewSize>>,
        results: Arc<Mutex<Results>>,
        capture: Arc<Mutex<Capture>>,
    }

    wrap_render_handler! {
        struct RenderHandlerImpl {
            shared: Shared,
        }

        impl RenderHandler {
            fn view_rect(
                &self,
                _browser: Option<&mut cef::Browser>,
                rect: Option<&mut cef::Rect>,
            ) {
                if let Some(rect) = rect {
                    let size = *self.shared.size.lock().unwrap();
                    rect.x = 0;
                    rect.y = 0;
                    rect.width = size.width;
                    rect.height = size.height;
                }
            }

            fn screen_info(
                &self,
                _browser: Option<&mut cef::Browser>,
                screen_info: Option<&mut cef::ScreenInfo>,
            ) -> ::std::os::raw::c_int {
                if let Some(screen_info) = screen_info {
                    // Report the egui pixels_per_point as the device scale so
                    // CEF renders into a physical-pixel backing buffer; egui
                    // then displays it 1:1 (crawler HiDPI fix). `view_rect`
                    // stays in DIP — CEF multiplies it by this factor.
                    screen_info.device_scale_factor = self.shared.size.lock().unwrap().scale;
                    return true as _;
                }
                false as _
            }

            fn on_paint(
                &self,
                _browser: Option<&mut cef::Browser>,
                _type_: cef::PaintElementType,
                _dirty_rects: Option<&[cef::Rect]>,
                buffer: *const u8,
                width: ::std::os::raw::c_int,
                height: ::std::os::raw::c_int,
            ) {
                if buffer.is_null() || width <= 0 || height <= 0 {
                    return;
                }
                let len = (width as usize) * (height as usize) * 4;
                // SAFETY: CEF guarantees `buffer` points to `width * height * 4`
                // BGRA bytes for the duration of this synchronous callback.
                let src = unsafe { std::slice::from_raw_parts(buffer, len) };
                let mut frame = self.shared.frame.lock().unwrap();
                frame.bgra.clear();
                frame.bgra.extend_from_slice(src);
                frame.width = width;
                frame.height = height;
                frame.dirty = true;
            }
        }
    }

    wrap_client! {
        struct ClientImpl {
            render_handler: cef::RenderHandler,
        }

        impl Client {
            fn render_handler(&self) -> Option<cef::RenderHandler> {
                Some(self.render_handler.clone())
            }
        }
    }

    wrap_dev_tools_message_observer! {
        struct DevToolsObserver {
            shared: Shared,
        }

        impl DevToolsMessageObserver {
            fn on_dev_tools_method_result(
                &self,
                _browser: Option<&mut cef::Browser>,
                message_id: ::std::os::raw::c_int,
                success: ::std::os::raw::c_int,
                result: Option<&[u8]>,
            ) {
                if success == 0 {
                    return;
                }
                let Some(bytes) = result else { return };
                // `Network.getResponseBody` results route into the capture by
                // their reserved message-id range; everything else is a
                // `Runtime.evaluate` (returnByValue:true) result whose payload
                // is `{"result":{"type":..,"value":<json>}}`.
                if message_id >= REQ_GET_BODY_BASE {
                    self.store_response_body(message_id, bytes);
                    return;
                }
                let Some(value) = parse_eval_value(bytes) else { return };
                let mut results = self.shared.results.lock().unwrap();
                match message_id {
                    REQ_RENDER_HTML => {
                        if let serde_json::Value::String(html) = value {
                            results.html = Some(html);
                        }
                    }
                    REQ_PICK => {
                        results.hit = parse_hit(&value);
                    }
                    _ => {}
                }
            }

            fn on_dev_tools_event(
                &self,
                _browser: Option<&mut cef::Browser>,
                method: Option<&cef::CefString>,
                params: Option<&[u8]>,
            ) {
                let (Some(method), Some(params)) = (method, params) else { return };
                let method = method.to_string();
                let Ok(params) = serde_json::from_slice::<serde_json::Value>(params) else {
                    return;
                };
                self.handle_network_event(&method, &params);
            }
        }
    }

    impl DevToolsObserver {
        /// Dispatch one CDP `Network` event into the shared capture
        /// (`crawler-warc-archive`). `responseReceived` records metadata;
        /// `loadingFinished` queues the body fetch the browser's `poll` issues.
        fn handle_network_event(&self, method: &str, params: &serde_json::Value) {
            match method {
                "Network.responseReceived" => {
                    if let Some((id, resp)) = parse_response_received(params) {
                        let mut cap = self.shared.capture.lock().unwrap();
                        let idx = cap.responses.len();
                        cap.responses.push(resp);
                        cap.by_request.insert(id, idx);
                    }
                }
                "Network.loadingFinished" => {
                    if let Some(id) = params.get("requestId").and_then(serde_json::Value::as_str) {
                        let mut cap = self.shared.capture.lock().unwrap();
                        if cap.by_request.contains_key(id) {
                            cap.bodies_to_fetch.push(id.to_owned());
                        }
                    }
                }
                _ => {}
            }
        }

        /// Store a `Network.getResponseBody` result against the response slot
        /// the message id was issued for. CDP returns `{"body":"..",
        /// "base64Encoded":bool}`; a base64 body is decoded, a plain one taken
        /// as UTF-8 bytes.
        fn store_response_body(&self, message_id: i32, bytes: &[u8]) {
            let Ok(payload) = serde_json::from_slice::<serde_json::Value>(bytes) else {
                return;
            };
            let body = decode_response_body(&payload);
            let mut cap = self.shared.capture.lock().unwrap();
            if let Some(&idx) = cap.pending_bodies.get(&message_id)
                && let Some(resp) = cap.responses.get_mut(idx)
            {
                resp.body = body;
            }
        }
    }

    /// Pull `.result.value` out of a CDP `Runtime.evaluate` result payload.
    fn parse_eval_value(bytes: &[u8]) -> Option<serde_json::Value> {
        let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        root.get("result")?.get("value").cloned()
    }

    /// Turn the picker eval's returned object into a [`Hit`]. The page-side JS
    /// returns `{ selectors: [..], text: "..", repeat: bool }`.
    fn parse_hit(value: &serde_json::Value) -> Option<Hit> {
        let selectors = value
            .get("selectors")?
            .as_array()?
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        if selectors.is_empty() {
            return None;
        }
        Some(Hit {
            selectors,
            text: value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            repeat: value
                .get("repeat")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Parse a CDP `Network.responseReceived` event into a captured response
    /// (`crawler-warc-archive`): the `requestId` plus the metadata from the
    /// nested `response` object (url, status, headers, mimeType). Returns
    /// `None` if the event is missing the request id or response object.
    fn parse_response_received(
        params: &serde_json::Value,
    ) -> Option<(String, crate::warc::CapturedResponse)> {
        let request_id = params.get("requestId")?.as_str()?.to_owned();
        let response = params.get("response")?;
        let headers = response
            .get("headers")
            .and_then(serde_json::Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(k, v)| {
                        let value = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                        (k.clone(), value)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let captured = crate::warc::CapturedResponse {
            url: response.get("url").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
            status: response.get("status").and_then(serde_json::Value::as_u64).unwrap_or(0) as u32,
            status_text: response
                .get("statusText")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            headers,
            mime_type: response
                .get("mimeType")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            body: Vec::new(),
        };
        Some((request_id, captured))
    }

    /// Decode a `Network.getResponseBody` payload's body, base64-decoding when
    /// CDP flagged the body as binary (`base64Encoded:true`) and otherwise
    /// taking the string as UTF-8 bytes.
    fn decode_response_body(payload: &serde_json::Value) -> Vec<u8> {
        let Some(body) = payload.get("body").and_then(serde_json::Value::as_str) else {
            return Vec::new();
        };
        let base64 = payload
            .get("base64Encoded")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if base64 {
            base64_decode(body).unwrap_or_default()
        } else {
            body.as_bytes().to_vec()
        }
    }

    /// Decode a standard (RFC 4648) base64 string with optional `=` padding.
    /// Hand-rolled to avoid a base64 crate dependency for this one call site;
    /// whitespace is ignored, any other invalid char aborts with `None`.
    fn base64_decode(input: &str) -> Option<Vec<u8>> {
        const fn val(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        let mut out = Vec::with_capacity(input.len() / 4 * 3);
        let mut acc: u32 = 0;
        let mut bits = 0u8;
        for &c in input.as_bytes() {
            if c == b'=' || c.is_ascii_whitespace() {
                continue;
            }
            let v = val(c)?;
            acc = (acc << 6) | u32::from(v);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Some(out)
    }

    /// One BGRA frame handed to the app: the pixels and their dimensions.
    pub struct PaintFrame {
        pub bgra: Vec<u8>,
        pub width: u32,
        pub height: u32,
    }

    /// A pointer button the app forwards to the page.
    #[derive(Clone, Copy)]
    pub enum PointerButton {
        Left,
        Middle,
        Right,
    }

    impl PointerButton {
        const fn to_cef(self) -> MouseButtonType {
            match self {
                Self::Left => MouseButtonType::LEFT,
                Self::Middle => MouseButtonType::MIDDLE,
                Self::Right => MouseButtonType::RIGHT,
            }
        }
    }

    /// The process-global CEF runtime (`crawler-cef-engine`): owns CEF init and
    /// the message-loop pump. Constructed exactly once, after
    /// [`subprocess_entry`] has returned in the browser process; CEF init is
    /// process-global and must NOT be called per tab. Per-tab browsers are
    /// created against the live runtime via [`CefRuntime::new_browser`].
    pub struct CefRuntime {
        /// Marker so the runtime can't be cloned/copied freely; its presence is
        /// the proof that `initialize` ran. Zero-sized.
        _private: (),
    }

    impl CefRuntime {
        /// Initialize CEF (once) and return the runtime handle.
        ///
        /// Must run on the main thread, after [`subprocess_entry`] has returned
        /// in the browser process. CEF runs single-threaded here, pumped by
        /// [`pump`](CefRuntime::pump). Calling this more than once is a bug —
        /// CEF init is process-global.
        #[must_use]
        pub fn new() -> Self {
            let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

            let args = Args::new();
            let settings = Settings {
                windowless_rendering_enabled: true as _,
                external_message_pump: true as _,
                no_sandbox: true as _,
                ..Default::default()
            };
            // The subprocess entry already returned -1 for the browser process,
            // so this is the browser process: bring CEF up.
            assert_eq!(
                initialize(Some(args.as_main_args()), Some(&settings), None, std::ptr::null_mut()),
                1,
                "cef::initialize failed"
            );
            Self { _private: () }
        }

        /// Pump CEF's message loop once for the whole process. Driven once per
        /// frame by the app (NOT per browser), since the loop is global. Only
        /// the active/visible tab's browser then issues
        /// [`begin_frame`](CefBrowser::begin_frame) to actually paint.
        #[expect(
            clippy::unused_self,
            reason = "the &self receiver proves CEF was initialized before pumping"
        )]
        pub fn pump(&self) {
            do_message_loop_work();
        }

        /// Create a new windowless OSR browser against the live runtime. One per
        /// workbench tab; multiple instances coexist.
        #[must_use]
        pub fn new_browser(&self) -> CefBrowser {
            CefBrowser::new()
        }
    }

    impl Default for CefRuntime {
        fn default() -> Self {
            Self::new()
        }
    }

    /// A single windowless CEF browser (`crawler-cef-engine`), one per workbench
    /// tab. `OnPaint` BGRA frames land in [`Shared`] and are drained by
    /// [`take_frame`](CefBrowser::take_frame) for egui upload; pointer/scroll/
    /// key input is forwarded straight to the browser host. Created via
    /// [`CefRuntime::new_browser`] (never before CEF init).
    pub struct CefBrowser {
        browser: Browser,
        shared: Shared,
        url: Option<String>,
        /// Keeps the DevTools message observer registered for the browser's
        /// lifetime; dropping it would detach the observer.
        _devtools: Option<Registration>,
        /// Whether the page was loading on the previous poll, so a
        /// loading→idle transition triggers an automatic rendered-HTML refresh.
        was_loading: bool,
    }

    impl CefBrowser {
        /// Create the windowless browser. Private: callers go through
        /// [`CefRuntime::new_browser`] so a browser can never be created before
        /// CEF init.
        fn new() -> Self {
            let shared = Shared::default();
            let window_info = WindowInfo {
                windowless_rendering_enabled: true as _,
                external_begin_frame_enabled: true as _,
                ..Default::default()
            };
            let browser_settings = BrowserSettings {
                windowless_frame_rate: 60,
                ..Default::default()
            };
            let render_handler = RenderHandlerImpl::new(shared.clone());
            let mut client = ClientImpl::new(render_handler);

            let browser = browser_host_create_browser_sync(
                Some(&window_info),
                Some(&mut client),
                Some(&CefString::from("about:blank")),
                Some(&browser_settings),
                None,
                None,
            )
            .expect("browser_host_create_browser_sync returned None");

            // Register the DevTools observer on the host so `Runtime.evaluate`
            // results route back to shared state (held for the browser's life).
            let devtools = browser.host().and_then(|host| {
                let mut observer = DevToolsObserver::new(shared.clone());
                host.add_dev_tools_message_observer(Some(&mut observer))
            });

            let this = Self {
                browser,
                shared,
                url: None,
                _devtools: devtools,
                was_loading: false,
            };
            // Enable the CDP Network domain so `responseReceived`/`loadingFinished`
            // events flow to the observer for WARC capture (crawler-warc-archive).
            this.dev_tools_call(REQ_NETWORK_ENABLE, "Network.enable");
            this
        }

        /// Take the latest BGRA frame if one was painted since the last call.
        /// Returns `None` when nothing changed (so the texture is left as-is).
        pub fn take_frame(&self) -> Option<PaintFrame> {
            let mut frame = self.shared.frame.lock().unwrap();
            if !frame.dirty || frame.bgra.is_empty() {
                return None;
            }
            frame.dirty = false;
            Some(PaintFrame {
                bgra: frame.bgra.clone(),
                width: frame.width as u32,
                height: frame.height as u32,
            })
        }

        /// Resize the off-screen view to the panel (logical px). Tells CEF the
        /// view changed so it re-lays-out and repaints at the new size.
        pub fn set_size(&self, width: i32, height: i32) {
            let width = width.max(1);
            let height = height.max(1);
            let mut size = self.shared.size.lock().unwrap();
            if size.width == width && size.height == height {
                return;
            }
            size.width = width;
            size.height = height;
            drop(size);
            if let Some(host) = self.browser.host() {
                host.was_resized();
            }
        }

        /// Update the HiDPI scale (the egui `pixels_per_point`) CEF reports as
        /// its `device_scale_factor`. When it changes, tell CEF the screen info
        /// changed so it re-renders into a physical-pixel backing buffer.
        pub fn set_scale(&self, scale: f32) {
            let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
            let mut size = self.shared.size.lock().unwrap();
            if (size.scale - scale).abs() < f32::EPSILON {
                return;
            }
            size.scale = scale;
            drop(size);
            if let Some(host) = self.browser.host() {
                host.notify_screen_info_changed();
                host.was_resized();
            }
        }

        /// Issue a CDP `Runtime.evaluate` of `expression`, routing the result to
        /// the shared slot keyed by `message_id` (see [`DevToolsObserver`]).
        fn eval(&self, message_id: i32, expression: &str) {
            let Some(host) = self.browser.host() else { return };
            let Some(params) = dictionary_value_create() else { return };
            params.set_string(
                Some(&CefString::from("expression")),
                Some(&CefString::from(expression)),
            );
            params.set_bool(Some(&CefString::from("returnByValue")), true as _);
            let mut params = params;
            host.execute_dev_tools_method(
                message_id,
                Some(&CefString::from("Runtime.evaluate")),
                Some(&mut params),
            );
        }

        /// Issue a parameter-less CDP method (e.g. `Network.enable`). The result
        /// (if any) routes to the shared slot for `message_id`.
        fn dev_tools_call(&self, message_id: i32, method: &str) {
            if let Some(host) = self.browser.host() {
                host.execute_dev_tools_method(message_id, Some(&CefString::from(method)), None);
            }
        }

        /// Issue `Network.getResponseBody` for one CDP `requestId`, routing the
        /// async result back to the response slot `message_id` was registered
        /// for (see [`Capture::pending_bodies`]).
        fn fetch_response_body(&self, message_id: i32, request_id: &str) {
            let Some(host) = self.browser.host() else { return };
            let Some(params) = dictionary_value_create() else { return };
            params.set_string(
                Some(&CefString::from("requestId")),
                Some(&CefString::from(request_id)),
            );
            let mut params = params;
            host.execute_dev_tools_method(
                message_id,
                Some(&CefString::from("Network.getResponseBody")),
                Some(&mut params),
            );
        }

        /// Drain the queue of finished requests, issuing a
        /// `Network.getResponseBody` for each and recording the message id so
        /// the result routes back. Called from `poll`, where a host is held.
        ///
        /// TODO(crawler-warc-archive): `getResponseBody` is async and only valid
        /// while the resource is still in CEF's per-navigation cache. A body
        /// whose round-trip lands after eviction (or after the next navigation
        /// reset) is simply skipped, so very large or slow-evicted resources may
        /// be missing from the archive. A robust fix needs CDP
        /// `Network.takeResponseBodyForInterceptionAsStream` or request-paused
        /// interception, which is a larger milestone.
        fn drain_body_fetches(&self) {
            let pending: Vec<(i32, String)> = {
                let mut cap = self.shared.capture.lock().unwrap();
                let drained: Vec<String> = std::mem::take(&mut cap.bodies_to_fetch);
                let mut out = Vec::with_capacity(drained.len());
                for request_id in drained {
                    let Some(&idx) = cap.by_request.get(&request_id) else { continue };
                    let message_id = REQ_GET_BODY_BASE + cap.next_body_slot;
                    cap.next_body_slot += 1;
                    cap.pending_bodies.insert(message_id, idx);
                    out.push((message_id, request_id));
                }
                out
            };
            for (message_id, request_id) in pending {
                self.fetch_response_body(message_id, &request_id);
            }
        }

        /// Forward a pointer move (logical px from the page origin).
        pub fn mouse_move(&self, x: f32, y: f32, left_down: bool) {
            if let Some(host) = self.browser.host() {
                host.send_mouse_move_event(Some(&mouse_event(x, y, left_down)), false as _);
            }
        }

        /// Forward a pointer button press/release.
        pub fn mouse_click(&self, x: f32, y: f32, button: PointerButton, pressed: bool) {
            if let Some(host) = self.browser.host() {
                host.send_mouse_click_event(
                    Some(&mouse_event(x, y, pressed)),
                    button.to_cef(),
                    (!pressed) as _,
                    1,
                );
            }
        }

        /// Forward a scroll wheel delta (logical px).
        pub fn mouse_wheel(&self, x: f32, y: f32, delta_x: f32, delta_y: f32) {
            if let Some(host) = self.browser.host() {
                host.send_mouse_wheel_event(
                    Some(&mouse_event(x, y, false)),
                    delta_x as i32,
                    delta_y as i32,
                );
            }
        }

        /// Forward a typed character to the focused field.
        pub fn key_char(&self, ch: char) {
            if let Some(host) = self.browser.host() {
                let code = ch as i32;
                let unit = u32::from(ch).min(u32::from(u16::MAX)) as u16;
                let event = KeyEvent {
                    type_: KeyEventType::CHAR,
                    windows_key_code: code,
                    native_key_code: code,
                    character: unit,
                    unmodified_character: unit,
                    ..Default::default()
                };
                host.send_key_event(Some(&event));
            }
        }

        /// Forward a raw key down/up (Windows virtual-key code, as CEF expects).
        pub fn key_raw(&self, windows_key_code: i32, pressed: bool) {
            if let Some(host) = self.browser.host() {
                let event = KeyEvent {
                    type_: if pressed { KeyEventType::KEYDOWN } else { KeyEventType::KEYUP },
                    windows_key_code,
                    native_key_code: windows_key_code,
                    ..Default::default()
                };
                host.send_key_event(Some(&event));
            }
        }

        /// Give (or remove) keyboard focus to the off-screen browser.
        pub fn set_focus(&self, focus: bool) {
            if let Some(host) = self.browser.host() {
                host.set_focus(focus as _);
            }
        }

        /// Outline every node matching `selector` on the page, for the side
        /// panel's hover re-highlight (`crawler-element-picker`). Injected as a
        /// single managed `<style>` element (id `__hiker_pick_hl`) whose rule is
        /// rebuilt each call, so repeated hovers don't stack styles. Cleared by
        /// [`clear_highlight`](Self::clear_highlight).
        ///
        /// TODO(crawler-element-picker): unverified without a live display — the
        /// inject/clear pair is structurally sound (idempotent style element,
        /// JSON-escaped selector), but real-page behavior (timing vs. SPA
        /// re-renders, selector edge cases) needs runtime testing.
        // status: crawler-element-picker
        pub fn highlight_selector(&self, selector: &str) {
            let Some(frame) = self.browser.main_frame() else { return };
            // Escape the selector as a JS string literal so quotes/backslashes
            // in the selector can't break out of the rule.
            let escaped = serde_json::Value::String(selector.to_owned()).to_string();
            let js = format!("{HIGHLIGHT_JS_PRELUDE}__hikerHl({escaped});");
            frame.execute_java_script(
                Some(&CefString::from(js.as_str())),
                Some(&CefString::from("hiker://pick-highlight")),
                0,
            );
        }

        /// Remove the re-highlight style element injected by
        /// [`highlight_selector`](Self::highlight_selector).
        pub fn clear_highlight(&self) {
            let Some(frame) = self.browser.main_frame() else { return };
            frame.execute_java_script(
                Some(&CefString::from(CLEAR_HIGHLIGHT_JS)),
                Some(&CefString::from("hiker://pick-highlight")),
                0,
            );
        }

        /// Snapshot the resources tapped off the CDP `Network` domain for the
        /// current navigation as `(url, body, mime)` triples, for backing the
        /// rendered preview's offline `ResourceProvider` (`crawler-render-preview`).
        /// The *same* wire responses the WARC archive (`crawler-warc-archive`) is
        /// assembled from also serve the preview's CSS/images, so the rendition
        /// matches what was captured. Entries with no URL or an unfetched body are
        /// dropped.
        pub fn captured_resources(&self) -> Vec<(String, Vec<u8>, String)> {
            let cap = self.shared.capture.lock().unwrap();
            cap.responses
                .iter()
                .filter(|r| !r.url.is_empty() && !r.body.is_empty())
                .map(|r| (r.url.clone(), r.body.clone(), r.mime_type.clone()))
                .collect()
        }

        /// Fire an async hit-test at a point (CSS px from the page origin) for
        /// the element picker (`crawler-element-picker`). Engine-specific input
        /// (a CDP `Runtime.evaluate` of the picker JS), like the mouse/key
        /// forwarders — so it's inherent here, not on the [`BrowserEngine`]
        /// trait. The async [`Hit`] is drained generically via
        /// [`BrowserEngine::take_hit`].
        pub fn request_pick(&mut self, x: f32, y: f32) {
            self.eval(REQ_PICK, &pick_js(x, y));
        }
    }

    impl BrowserEngine for CefBrowser {
        fn load(&mut self, url: &str) {
            if let Some(frame) = self.browser.main_frame() {
                frame.load_url(Some(&CefString::from(url)));
            }
            self.url = Some(url.to_owned());
            // Drop the previous page's cached render so a crawl fetch over a
            // reused browser doesn't read a stale snapshot before the new
            // page's HTML lands (crawler-crawl-run).
            self.shared.results.lock().unwrap().html = None;
            // Start a fresh capture for the new navigation so the archive only
            // covers the page in view (crawler-warc-archive).
            self.shared.capture.lock().unwrap().reset(url);
        }

        fn poll(&mut self) {
            // The GLOBAL message loop is pumped once per frame by
            // `CefRuntime::pump`; here we only do this browser's per-frame work.
            // Ask the off-screen browser for a frame so `OnPaint` fires
            // (external begin-frame is enabled above). Only the active/visible
            // tab is `poll`ed each frame, so background tabs stay paused.
            if let Some(host) = self.browser.host() {
                host.send_external_begin_frame();
            }
            // Issue any queued `getResponseBody` fetches for finished requests
            // (the observer can't — it has no host) so bodies land before
            // eviction (crawler-warc-archive).
            self.drain_body_fetches();
            // Auto-refresh the cached rendered HTML on a load→idle transition,
            // so previews/manifest see the post-JS DOM without an explicit kick.
            let loading = self.browser.is_loading() != 0;
            if self.was_loading && !loading {
                self.request_render_html();
            }
            self.was_loading = loading;
        }

        fn current_url(&self) -> Option<&str> {
            self.url.as_deref()
        }

        fn rendered_html(&self) -> Option<String> {
            self.shared.results.lock().unwrap().html.clone()
        }

        fn request_render_html(&mut self) {
            self.eval(REQ_RENDER_HTML, "document.documentElement.outerHTML");
        }

        fn take_hit(&mut self) -> Option<Hit> {
            self.shared.results.lock().unwrap().hit.take()
        }

        /// Assemble the responses tapped off CDP's `Network` domain for the
        /// current navigation into a WARC archive (`crawler-warc-archive`).
        /// Bodies whose `getResponseBody` round-trip didn't land in time are
        /// emitted with an empty body (see the timing TODO on
        /// [`Self::drain_body_fetches`]). `None` when nothing was captured.
        fn capture_warc(&self) -> Option<Vec<u8>> {
            let cap = self.shared.capture.lock().unwrap();
            crate::warc::assemble(&cap.page_url, &cap.responses)
        }
    }

    /// Build the picker `Runtime.evaluate` expression: an IIFE that hit-tests
    /// `(x, y)` in CSS px, ranks stable selectors for the hit node, and returns
    /// `{ selectors, text, repeat }` (CDP delivers it as a structured value
    /// because `returnByValue` is set).
    fn pick_js(x: f32, y: f32) -> String {
        format!("(function(){{{PICK_JS_BODY}}})({x}, {y})")
    }

    /// The picker hit-test + selector-ranking body, evaluated against the page.
    /// Coordinates arrive as the IIFE's `x`, `y` arguments (CSS px). Ranks
    /// candidates most-stable first: unique `#id` → a unique single class → a
    /// stable attribute (`itemprop`/`data-*`) → a bounded `nth-child` path.
    const PICK_JS_BODY: &str = r#"
var x=arguments[0], y=arguments[1];
var el=document.elementFromPoint(x,y);
if(!el){return null;}
function uniq(sel){try{return document.querySelectorAll(sel).length===1;}catch(e){return false;}}
function cssEsc(s){return (window.CSS&&CSS.escape)?CSS.escape(s):String(s).replace(/[^a-zA-Z0-9_-]/g,'\\$&');}
var sels=[];
if(el.id&&uniq('#'+cssEsc(el.id))){sels.push('#'+cssEsc(el.id));}
var cls=(el.className&&el.className.baseVal!==undefined)?el.className.baseVal:(typeof el.className==='string'?el.className:'');
(cls?cls.trim().split(/\s+/):[]).forEach(function(c){if(!c){return;}var s='.'+cssEsc(c);if(uniq(s)){sels.push(s);}});
['itemprop','data-testid','data-id','data-qa','name','aria-label'].forEach(function(a){var v=el.getAttribute&&el.getAttribute(a);if(v){var s=el.tagName.toLowerCase()+'['+a+'="'+v.replace(/"/g,'\\"')+'"]';if(uniq(s)){sels.push(s);}}});
function pathOf(node){var parts=[];var cur=node;var depth=0;while(cur&&cur.nodeType===1&&depth<6){var tag=cur.tagName.toLowerCase();if(cur.id){parts.unshift('#'+cssEsc(cur.id));break;}var p=cur.parentElement;if(p){var idx=1;var sib=cur;while((sib=sib.previousElementSibling)){if(sib.tagName===cur.tagName){idx++;}}tag+=':nth-of-type('+idx+')';}parts.unshift(tag);cur=cur.parentElement;depth++;}return parts.join(' > ');}
var path=pathOf(el);
if(sels.indexOf(path)===-1){sels.push(path);}
var txt=(el.innerText||el.textContent||'').trim();
if(txt.length>500){txt=txt.slice(0,500);}
var repeat=false;
if(sels.length){try{repeat=document.querySelectorAll(sels[0]).length>1;}catch(e){repeat=false;}}
return {selectors:sels, text:txt, repeat:repeat};
"#;

    /// Defines `__hikerHl(sel)`: ensure a managed `<style id=__hiker_pick_hl>`
    /// exists and set its rule to outline every node matching `sel`. Using a
    /// CSS rule (rather than per-node inline styles) means clearing is a single
    /// element removal and SPA re-renders don't strip the highlight.
    const HIGHLIGHT_JS_PRELUDE: &str = r#"
window.__hikerHl=function(sel){
  try{
    var id='__hiker_pick_hl';
    var st=document.getElementById(id);
    if(!st){st=document.createElement('style');st.id=id;(document.head||document.documentElement).appendChild(st);}
    st.textContent=sel+'{outline:2px solid #e8590c !important;outline-offset:1px !important;}';
  }catch(e){}
};
"#;

    /// Remove the managed re-highlight style element if present.
    const CLEAR_HIGHLIGHT_JS: &str =
        "(function(){var s=document.getElementById('__hiker_pick_hl');if(s){s.remove();}})();";

    /// Build a CEF mouse event at a logical point, with the left-button flag set
    /// in the modifier mask when the button is held (drag/selection).
    const fn mouse_event(x: f32, y: f32, left_down: bool) -> MouseEvent {
        MouseEvent {
            x: x as i32,
            y: y as i32,
            modifiers: if left_down {
                sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON.0
            } else {
                0
            },
        }
    }

    /// CEF's self-exec subprocess gate. CEF re-launches this same binary as its
    /// helper processes (renderer/GPU/utility); each must run CEF's process
    /// executor and exit immediately rather than starting the egui app. Call
    /// this FIRST in `main`: it returns `true` in the browser (main) process so
    /// the caller proceeds to build the app, and exits the process directly for
    /// any helper. Mirrors the `osr`/`cefsimple` examples' entry pattern.
    #[must_use]
    pub fn subprocess_entry() -> bool {
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = Args::new();
        let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
        if ret >= 0 {
            // A helper process handled its work; do not start the app.
            std::process::exit(ret);
        }
        // ret == -1: this is the browser process.
        true
    }
}
