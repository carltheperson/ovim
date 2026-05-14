//! Experimental AX-based Vim eligibility tracking.
//!
//! Thesis under test:
//! Cursor's AI/sidebar/composer input can be distinguished from the normal
//! Monaco editor by combining focused AX text-editing signals with nearby
//! DOM/AX class and identifier evidence. In Cursor we prefer false negatives:
//! unknown or weak evidence is treated as not allowed.
//!
//! This module is always available for transition logging in normal ovim logs.
//! In this fork it is also the hardcoded activation policy:
//! - Kitty is never eligible.
//! - Cursor is eligible only when the classifier returns Allowed.
//! - Other apps are eligible.

use std::ffi::CStr;
use std::os::raw::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use serde::Serialize;
use tauri::Emitter;

use crate::config::VimEligibilitySettings;

const CACHE_TTL_MS: u64 = 500;
const MAX_ANCESTORS: usize = 10;

#[allow(non_upper_case_globals)]
const kAXValueCGPointType: i32 = 1;
#[allow(non_upper_case_globals)]
const kAXValueCGSizeType: i32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyAttributeNames(element: CFTypeRef, names: *mut CFTypeRef) -> i32;
    fn AXValueGetValue(value: CFTypeRef, the_type: i32, value_ptr: *mut c_void) -> bool;
    fn AXObserverCreate(
        application: i32,
        callback: extern "C" fn(CFTypeRef, CFTypeRef, CFStringRef, *mut c_void),
        out_observer: *mut CFTypeRef,
    ) -> i32;
    fn AXObserverAddNotification(
        observer: CFTypeRef,
        element: CFTypeRef,
        notification: CFTypeRef,
        refcon: *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(observer: CFTypeRef) -> CFTypeRef;
    fn CFRunLoopGetMain() -> CFTypeRef;
    fn CFRunLoopAddSource(rl: CFTypeRef, source: CFTypeRef, mode: CFTypeRef);
}

struct RetainedCf(CFTypeRef);

impl RetainedCf {
    fn new(ptr: CFTypeRef) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }
}

impl Drop for RetainedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

unsafe impl Send for RetainedCf {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusCheckReason {
    AxFocusChanged,
    FrontmostAppChanged,
    PreNormalModeCheck,
    PreKeyInterceptCheck,
    ManualCommand,
    SelectionUpdate,
    PassThroughKey,
}

impl FocusCheckReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::AxFocusChanged => "AXFocusedUIElementChangedNotification",
            Self::FrontmostAppChanged => "frontmost-app-changed",
            Self::PreNormalModeCheck => "pre-normal-mode-check",
            Self::PreKeyInterceptCheck => "pre-key-intercept-check",
            Self::ManualCommand => "manual-command",
            Self::SelectionUpdate => "selection-update",
            Self::PassThroughKey => "pass-through-key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimEligibility {
    Allowed,
    NotAllowed,
    Unknown,
}

impl VimEligibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "Allowed",
            Self::NotAllowed => "NotAllowed",
            Self::Unknown => "Unknown",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Allowed => "A",
            Self::NotAllowed => "_",
            Self::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EligibilitySnapshot {
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub eligibility: VimEligibility,
    pub evidence: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EligibilityChangeEvent {
    pub eligibility: String,
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
struct ElementMetadata {
    role: Option<String>,
    role_description: Option<String>,
    title: Option<String>,
    description: Option<String>,
    help: Option<String>,
    placeholder: Option<String>,
    identifier: Option<String>,
    dom_identifier: Option<String>,
    dom_class_list: Vec<String>,
    editable: Option<bool>,
    focused: Option<bool>,
    frame: Option<ElementFrame>,
    supports_selected_text_range: bool,
}

#[derive(Debug, Clone)]
struct ElementFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone)]
struct RawFocusSnapshot {
    app_name: Option<String>,
    bundle_id: Option<String>,
    focused: Option<ElementMetadata>,
    ancestors: Vec<ElementMetadata>,
}

#[derive(Clone)]
struct CachedEligibility {
    snapshot: EligibilitySnapshot,
    captured_at: Instant,
}

struct EligibilityState {
    log_transitions: bool,
    verbose_ax_focus: bool,
    cache: Option<CachedEligibility>,
    previous: Option<VimEligibility>,
    observed_pid: Option<i32>,
}

static STATE: OnceLock<Mutex<EligibilityState>> = OnceLock::new();
static AX_OBSERVERS: OnceLock<Mutex<Vec<RetainedCf>>> = OnceLock::new();

pub fn init(settings: &VimEligibilitySettings) {
    let log_transitions = settings.log_transitions || env_flag("OVIM_LOG_VIM_ELIGIBILITY");
    let verbose_ax_focus = settings.verbose_ax_focus || env_flag("OVIM_VERBOSE_AX_FOCUS");

    let state = EligibilityState {
        log_transitions,
        verbose_ax_focus,
        cache: None,
        previous: None,
        observed_pid: None,
    };

    if STATE.set(Mutex::new(state)).is_err() {
        update_settings(log_transitions, verbose_ax_focus);
        return;
    }

    log::info!(
        "[ovim eligibility] started log_transitions={} policy=kitty-blocked,cursor-classified,others-allowed verbose_ax_focus={}",
        log_transitions,
        verbose_ax_focus
    );
    recompute(FocusCheckReason::ManualCommand);
    start_ax_focus_observer_for_current_app();
    start_frontmost_app_observer();
}

pub fn update_from_settings(settings: &VimEligibilitySettings) {
    update_settings(
        settings.log_transitions || env_flag("OVIM_LOG_VIM_ELIGIBILITY"),
        settings.verbose_ax_focus || env_flag("OVIM_VERBOSE_AX_FOCUS"),
    );
}

pub fn should_log_pass_through_key() -> bool {
    STATE
        .get()
        .and_then(|state| state.lock().ok().map(|guard| guard.verbose_ax_focus))
        .unwrap_or(false)
}

pub fn allows_vim_at_decision_point(reason: FocusCheckReason) -> bool {
    let Some(snapshot) = recompute_if_needed(reason) else {
        return true;
    };

    snapshot.eligibility == VimEligibility::Allowed
}

pub fn current_eligibility() -> VimEligibility {
    STATE
        .get()
        .and_then(|state| {
            let guard = state.lock().ok()?;
            Some(guard.cache.as_ref()?.snapshot.eligibility)
        })
        .unwrap_or(VimEligibility::Unknown)
}

pub fn current_eligibility_label() -> &'static str {
    current_eligibility().short_label()
}

pub fn recompute(reason: FocusCheckReason) -> Option<EligibilitySnapshot> {
    let raw = capture_raw_focus_snapshot();
    let snapshot = classify(raw.as_ref());
    store_and_log(snapshot.clone(), reason, "fresh-query");
    Some(snapshot)
}

pub fn recompute_if_needed(reason: FocusCheckReason) -> Option<EligibilitySnapshot> {
    let now = Instant::now();
    let cached = STATE.get().and_then(|state| {
        let guard = state.lock().ok()?;
        guard.cache.clone()
    });

    if let Some(cached) = cached {
        let age_ms = now.duration_since(cached.captured_at).as_millis();
        if age_ms <= u128::from(CACHE_TTL_MS) {
            maybe_log_transition(&cached.snapshot, reason, "cache", Some(age_ms));
            return Some(cached.snapshot);
        }
    }

    recompute(reason)
}

pub fn invalidate_cache(_reason: FocusCheckReason) {
    if let Some(state) = STATE.get() {
        if let Ok(mut guard) = state.lock() {
            guard.cache = None;
        }
    }
}

fn update_settings(log_transitions: bool, verbose_ax_focus: bool) {
    if let Some(state) = STATE.get() {
        if let Ok(mut guard) = state.lock() {
            guard.log_transitions = log_transitions;
            guard.verbose_ax_focus = verbose_ax_focus;
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn store_and_log(snapshot: EligibilitySnapshot, reason: FocusCheckReason, source: &'static str) {
    if let Some(state) = STATE.get() {
        if let Ok(mut guard) = state.lock() {
            guard.cache = Some(CachedEligibility {
                snapshot: snapshot.clone(),
                captured_at: Instant::now(),
            });
        }
    }

    maybe_log_transition(&snapshot, reason, source, None);
    maybe_log_verbose_ax(reason);
}

fn maybe_log_transition(
    snapshot: &EligibilitySnapshot,
    reason: FocusCheckReason,
    source: &'static str,
    cache_age_ms: Option<u128>,
) {
    let Some(state) = STATE.get() else {
        return;
    };

    let mut should_log = false;
    let mut previous = None;
    if let Ok(mut guard) = state.lock() {
        if !guard.log_transitions {
            return;
        }
        previous = guard.previous;
        if previous != Some(snapshot.eligibility) {
            guard.previous = Some(snapshot.eligibility);
            should_log = true;
        }
    }

    if !should_log {
        return;
    }

    log::info!(
        "[ovim eligibility]\napp={}\nbundle_id={}\nreason={}\nsource={}\ncache_age_ms={}\neligibility={} previous={}\nsummary={}\nevidence={:?}",
        log_value(snapshot.app_name.as_deref()),
        log_value(snapshot.bundle_id.as_deref()),
        reason.as_str(),
        source,
        cache_age_ms
            .map(|age| age.to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        snapshot.eligibility.as_str(),
        previous
            .map(VimEligibility::as_str)
            .unwrap_or("<none>"),
        snapshot.reason,
        snapshot.evidence
    );

    if let Some(app) = crate::get_app_handle() {
        let _ = app.emit(
            "vim-eligibility-change",
            EligibilityChangeEvent {
                eligibility: snapshot.eligibility.as_str().to_string(),
                app_name: snapshot.app_name.clone(),
                bundle_id: snapshot.bundle_id.clone(),
                reason: snapshot.reason.clone(),
            },
        );
    }
}

fn maybe_log_verbose_ax(reason: FocusCheckReason) {
    let verbose = STATE
        .get()
        .and_then(|state| state.lock().ok().map(|guard| guard.verbose_ax_focus))
        .unwrap_or(false);
    if !verbose {
        return;
    }

    if let Some(raw) = capture_raw_focus_snapshot() {
        log::info!(
            "[ovim eligibility verbose]\nreason={}\napp={}\nbundle_id={}\nfocused={}\nancestors={:?}",
            reason.as_str(),
            log_value(raw.app_name.as_deref()),
            log_value(raw.bundle_id.as_deref()),
            raw.focused
                .as_ref()
                .map(format_element_summary)
                .unwrap_or_else(|| "<none>".to_string()),
            raw.ancestors
                .iter()
                .enumerate()
                .map(|(index, element)| format!("{}: {}", index, format_element_summary(element)))
                .collect::<Vec<_>>()
        );
    }
}

fn log_value(value: Option<&str>) -> String {
    match value {
        Some("") => "<empty>".to_string(),
        Some(value) => value.to_string(),
        None => "<none>".to_string(),
    }
}

fn classify(raw: Option<&RawFocusSnapshot>) -> EligibilitySnapshot {
    let Some(raw) = raw else {
        return EligibilitySnapshot {
            app_name: None,
            bundle_id: None,
            eligibility: VimEligibility::Unknown,
            evidence: vec!["AX query failed or focused element unavailable".to_string()],
            reason: "AX query failed".to_string(),
        };
    };

    let is_cursor = is_cursor_or_vscode(raw.bundle_id.as_deref());
    if is_kitty(raw.bundle_id.as_deref(), raw.app_name.as_deref()) {
        return EligibilitySnapshot {
            app_name: raw.app_name.clone(),
            bundle_id: raw.bundle_id.clone(),
            eligibility: VimEligibility::NotAllowed,
            evidence: vec!["hardcoded fork policy: Kitty is never eligible".to_string()],
            reason: "hardcoded Kitty blocklist".to_string(),
        };
    }

    if !is_cursor {
        return EligibilitySnapshot {
            app_name: raw.app_name.clone(),
            bundle_id: raw.bundle_id.clone(),
            eligibility: VimEligibility::Allowed,
            evidence: vec!["hardcoded fork policy: non-Cursor, non-Kitty apps are eligible".to_string()],
            reason: "hardcoded non-Cursor allowlist".to_string(),
        };
    }

    classify_cursor(raw)
}

fn classify_cursor(raw: &RawFocusSnapshot) -> EligibilitySnapshot {
    let mut evidence = vec!["frontmost app is Cursor/VS Code".to_string()];
    let mut text_like_score = 0;
    let mut positive_sidebar_score = 0;
    let mut negative_score = 0;

    for (depth, element) in elements_by_depth(raw).iter().enumerate() {
        let weight = depth_weight(depth);
        let is_focused = depth == 0;

        if is_text_like(element) {
            let score = if is_focused { 5 } else { weight };
            text_like_score += score;
            evidence.push(format!(
                "{} text-like AX element at depth {} (+{})",
                if is_focused { "focused" } else { "near" },
                depth,
                score
            ));
        }

        for marker in element_tokens(element) {
            let token = marker.to_lowercase();

            if let Some(label) = positive_sidebar_marker(&token) {
                let score = if label == "input" { weight } else { weight + 2 };
                positive_sidebar_score += score;
                evidence.push(format!(
                    "positive marker '{}' matched at depth {} (+{})",
                    label, depth, score
                ));
            }

            if let Some(label) = negative_editor_or_terminal_marker(&token) {
                let score = weight + 4;
                negative_score += score;
                evidence.push(format!(
                    "negative marker '{}' matched at depth {} (+{})",
                    label, depth, score
                ));
            }
        }
    }

    evidence.push(format!("text_like_score={}", text_like_score));
    evidence.push(format!("positive_sidebar_score={}", positive_sidebar_score));
    evidence.push(format!("negative_editor_or_terminal_score={}", negative_score));

    let eligibility = if negative_score >= 6 {
        VimEligibility::NotAllowed
    } else if text_like_score >= 5 && positive_sidebar_score >= 5 {
        VimEligibility::Allowed
    } else {
        VimEligibility::NotAllowed
    };

    let reason = match eligibility {
        VimEligibility::Allowed => {
            "focused text-like element with nearby composer/sidebar input evidence".to_string()
        }
        VimEligibility::NotAllowed if negative_score >= 6 => {
            "matched editor/terminal context near focused element".to_string()
        }
        VimEligibility::NotAllowed => {
            "no confident positive composer/sidebar evidence in Cursor".to_string()
        }
        VimEligibility::Unknown => "insufficient AX data".to_string(),
    };

    EligibilitySnapshot {
        app_name: raw.app_name.clone(),
        bundle_id: raw.bundle_id.clone(),
        eligibility,
        evidence,
        reason,
    }
}

fn is_cursor_or_vscode(bundle_id: Option<&str>) -> bool {
    let Some(bundle_id) = bundle_id else {
        return false;
    };
    let normalized = bundle_id.to_lowercase();
    normalized.contains("todesktop")
        || normalized.contains("cursor")
        || normalized.contains("vscode")
        || normalized.contains("visual-studio-code")
}

fn is_kitty(bundle_id: Option<&str>, app_name: Option<&str>) -> bool {
    let bundle_id = bundle_id.unwrap_or("").to_lowercase();
    let app_name = app_name.unwrap_or("").to_lowercase();
    bundle_id == "net.kovidgoyal.kitty"
        || bundle_id.contains("kitty")
        || app_name == "kitty"
}

fn elements_by_depth(raw: &RawFocusSnapshot) -> Vec<&ElementMetadata> {
    let mut elements = Vec::new();
    if let Some(focused) = &raw.focused {
        elements.push(focused);
    }
    elements.extend(raw.ancestors.iter());
    elements
}

fn depth_weight(depth: usize) -> i32 {
    match depth {
        0 => 6,
        1..=5 => 5,
        6..=10 => 2,
        _ => 0,
    }
}

fn is_text_like(element: &ElementMetadata) -> bool {
    matches!(
        element.role.as_deref(),
        Some("AXTextArea" | "AXTextField" | "AXSearchField" | "AXComboBox")
    ) || element
        .role_description
        .as_deref()
        .is_some_and(|description| {
            let normalized = description.to_lowercase();
            normalized.contains("text entry") || normalized.contains("text area")
        })
        || element.supports_selected_text_range
        || element.editable == Some(true)
}

fn positive_sidebar_marker(token: &str) -> Option<&'static str> {
    if token.starts_with("ai-input") {
        Some("ai-input-*")
    } else if token.contains("composer") {
        Some("composer")
    } else if token.contains("aislash") {
        Some("aislash")
    } else if token.contains("chat") {
        Some("chat")
    } else if token.contains("prompt") {
        Some("prompt")
    } else if token.contains("ask") {
        Some("ask")
    } else if token.contains("input") {
        Some("input")
    } else {
        None
    }
}

fn negative_editor_or_terminal_marker(token: &str) -> Option<&'static str> {
    if token.contains("monaco-diff-editor") {
        Some("monaco-diff-editor")
    } else if token.contains("monaco-editor") {
        Some("monaco-editor")
    } else if token.contains("view-lines") {
        Some("view-lines")
    } else if token.contains("view-line") {
        Some("view-line")
    } else if token.contains("editor-instance") {
        Some("editor-instance")
    } else if token.contains("textarea.inputarea") || token.contains("inputarea") {
        Some("textarea.inputarea")
    } else if token.contains("xterm") {
        Some("xterm")
    } else if token.contains("terminal-instance") {
        Some("terminal-instance")
    } else if token.contains("terminal-wrapper") {
        Some("terminal-wrapper")
    } else if token.contains("terminal") {
        Some("terminal")
    } else {
        None
    }
}

fn element_tokens(element: &ElementMetadata) -> Vec<String> {
    let mut tokens = Vec::new();
    for value in [
        &element.role,
        &element.role_description,
        &element.title,
        &element.description,
        &element.help,
        &element.placeholder,
        &element.identifier,
        &element.dom_identifier,
    ]
    .into_iter()
    .flatten()
    {
        tokens.push(value.clone());
    }
    tokens.extend(element.dom_class_list.iter().cloned());
    tokens
}

fn capture_raw_focus_snapshot() -> Option<RawFocusSnapshot> {
    let app = frontmost_application_info();
    let system_wide = RetainedCf::new(unsafe { AXUIElementCreateSystemWide() })?;
    let focused_app = get_attribute(system_wide.0, "AXFocusedApplication")?;
    let focused_element = get_attribute(focused_app.0, "AXFocusedUIElement")?;

    let focused = Some(element_metadata(focused_element.0));
    let mut ancestors = Vec::new();
    let mut current = focused_element;

    for _ in 0..MAX_ANCESTORS {
        let Some(parent) = get_attribute(current.0, "AXParent") else {
            break;
        };
        ancestors.push(element_metadata(parent.0));
        current = parent;
    }

    Some(RawFocusSnapshot {
        app_name: app.name,
        bundle_id: app.bundle_id,
        focused,
        ancestors,
    })
}

fn get_attribute(element: CFTypeRef, attr_name: &str) -> Option<RetainedCf> {
    let attr = CFString::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let result = unsafe { AXUIElementCopyAttributeValue(element, attr.as_CFTypeRef(), &mut value) };
    if result != 0 || value.is_null() {
        None
    } else {
        RetainedCf::new(value)
    }
}

fn element_metadata(element: CFTypeRef) -> ElementMetadata {
    let attributes = supported_attributes(element);
    let position = get_attribute(element, "AXPosition").and_then(|handle| extract_point(handle.0));
    let size = get_attribute(element, "AXSize").and_then(|handle| extract_size(handle.0));
    let frame = match (position, size) {
        (Some((x, y)), Some((width, height))) => Some(ElementFrame {
            x,
            y,
            width,
            height,
        }),
        _ => None,
    };

    ElementMetadata {
        role: get_string_attribute(element, "AXRole"),
        role_description: get_string_attribute(element, "AXRoleDescription"),
        title: get_string_attribute(element, "AXTitle"),
        description: get_string_attribute(element, "AXDescription"),
        help: get_string_attribute(element, "AXHelp"),
        placeholder: get_string_attribute(element, "AXPlaceholderValue"),
        identifier: get_string_attribute(element, "AXIdentifier"),
        dom_identifier: get_string_attribute(element, "AXDOMIdentifier"),
        dom_class_list: get_string_array_attribute(element, "AXDOMClassList"),
        editable: get_bool_attribute(element, "AXEditable"),
        focused: get_bool_attribute(element, "AXFocused"),
        frame,
        supports_selected_text_range: attributes.iter().any(|attr| attr == "AXSelectedTextRange"),
    }
}

fn get_string_attribute(element: CFTypeRef, attr_name: &str) -> Option<String> {
    let handle = get_attribute(element, attr_name)?;
    if unsafe { CFGetTypeID(handle.0) } != unsafe { core_foundation::string::CFStringGetTypeID() } {
        return None;
    }

    let cf_string: CFString = unsafe { CFString::wrap_under_create_rule(handle.0 as _) };
    let result = cf_string.to_string();
    std::mem::forget(handle);
    Some(result)
}

fn get_bool_attribute(element: CFTypeRef, attr_name: &str) -> Option<bool> {
    let handle = get_attribute(element, attr_name)?;
    let true_value = unsafe { core_foundation::boolean::kCFBooleanTrue as CFTypeRef };
    let false_value = unsafe { core_foundation::boolean::kCFBooleanFalse as CFTypeRef };
    if handle.0 == true_value {
        Some(true)
    } else if handle.0 == false_value {
        Some(false)
    } else {
        None
    }
}

fn get_string_array_attribute(element: CFTypeRef, attr_name: &str) -> Vec<String> {
    let Some(handle) = get_attribute(element, attr_name) else {
        return Vec::new();
    };
    if unsafe { CFGetTypeID(handle.0) } != unsafe { core_foundation::array::CFArrayGetTypeID() } {
        return Vec::new();
    }

    let count = unsafe { core_foundation::array::CFArrayGetCount(handle.0 as _) };
    let mut values = Vec::new();
    for index in 0..count {
        let value = unsafe { core_foundation::array::CFArrayGetValueAtIndex(handle.0 as _, index) };
        if value.is_null() {
            continue;
        }
        if unsafe { CFGetTypeID(value as CFTypeRef) }
            != unsafe { core_foundation::string::CFStringGetTypeID() }
        {
            continue;
        }
        let cf_string: CFString = unsafe { CFString::wrap_under_get_rule(value as _) };
        values.push(cf_string.to_string());
    }
    values
}

fn supported_attributes(element: CFTypeRef) -> Vec<String> {
    let mut names: CFTypeRef = std::ptr::null();
    let result = unsafe { AXUIElementCopyAttributeNames(element, &mut names) };
    let Some(handle) = (if result == 0 { RetainedCf::new(names) } else { None }) else {
        return Vec::new();
    };

    let count = unsafe { core_foundation::array::CFArrayGetCount(handle.0 as _) };
    let mut attributes = Vec::new();
    for index in 0..count {
        let value = unsafe { core_foundation::array::CFArrayGetValueAtIndex(handle.0 as _, index) };
        if value.is_null() {
            continue;
        }
        if unsafe { CFGetTypeID(value as CFTypeRef) }
            != unsafe { core_foundation::string::CFStringGetTypeID() }
        {
            continue;
        }
        let cf_string: CFString = unsafe { CFString::wrap_under_get_rule(value as _) };
        attributes.push(cf_string.to_string());
    }
    attributes
}

fn extract_point(value: CFTypeRef) -> Option<(f64, f64)> {
    let mut point = core_graphics::geometry::CGPoint::new(0.0, 0.0);
    let extracted = unsafe {
        AXValueGetValue(
            value,
            kAXValueCGPointType,
            &mut point as *mut _ as *mut c_void,
        )
    };
    extracted.then_some((point.x, point.y))
}

fn extract_size(value: CFTypeRef) -> Option<(f64, f64)> {
    let mut size = core_graphics::geometry::CGSize::new(0.0, 0.0);
    let extracted = unsafe {
        AXValueGetValue(
            value,
            kAXValueCGSizeType,
            &mut size as *mut _ as *mut c_void,
        )
    };
    extracted.then_some((size.width, size.height))
}

struct FrontmostAppInfo {
    name: Option<String>,
    bundle_id: Option<String>,
    pid: Option<i32>,
}

fn frontmost_application_info() -> FrontmostAppInfo {
    unsafe {
        use objc::{class, msg_send, sel, sel_impl};

        let workspace: *mut objc::runtime::Object =
            msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return FrontmostAppInfo {
                name: None,
                bundle_id: None,
                pid: None,
            };
        }

        let app: *mut objc::runtime::Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return FrontmostAppInfo {
                name: None,
                bundle_id: None,
                pid: None,
            };
        }

        FrontmostAppInfo {
            name: ns_string_to_string(msg_send![app, localizedName]),
            bundle_id: ns_string_to_string(msg_send![app, bundleIdentifier]),
            pid: Some(msg_send![app, processIdentifier]),
        }
    }
}

unsafe fn ns_string_to_string(value: *mut objc::runtime::Object) -> Option<String> {
    use objc::{msg_send, sel, sel_impl};

    if value.is_null() {
        return None;
    }
    let utf8: *const std::os::raw::c_char = msg_send![value, UTF8String];
    if utf8.is_null() {
        None
    } else {
        Some(CStr::from_ptr(utf8).to_string_lossy().into_owned())
    }
}

fn format_element_summary(element: &ElementMetadata) -> String {
    format!(
        "role={} role_description={} identifier={} dom_identifier={} classes={:?} editable={} focused={} frame={}",
        log_value(element.role.as_deref()),
        log_value(element.role_description.as_deref()),
        log_value(element.identifier.as_deref()),
        log_value(element.dom_identifier.as_deref()),
        element.dom_class_list,
        element
            .editable
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        element
            .focused
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string()),
        element
            .frame
            .as_ref()
            .map(|frame| format!(
                "{{x={:.1}, y={:.1}, w={:.1}, h={:.1}}}",
                frame.x, frame.y, frame.width, frame.height
            ))
            .unwrap_or_else(|| "<none>".to_string())
    )
}

fn start_frontmost_app_observer() {
    use dispatch::Queue;
    use objc::{class, msg_send, sel, sel_impl};

    Queue::main().exec_async(move || unsafe {
        let workspace: *mut objc::runtime::Object =
            msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            log::warn!("[ovim eligibility] failed to get NSWorkspace for frontmost-app observer");
            return;
        }

        let notification_center: *mut objc::runtime::Object =
            msg_send![workspace, notificationCenter];
        if notification_center.is_null() {
            log::warn!("[ovim eligibility] failed to get NSWorkspace notification center");
            return;
        }

        let block = block::ConcreteBlock::new(move |_notification: *mut objc::runtime::Object| {
            invalidate_cache(FocusCheckReason::FrontmostAppChanged);
            recompute(FocusCheckReason::FrontmostAppChanged);
            start_ax_focus_observer_for_current_app();
        });
        let block = block.copy();

        let notification_name: *mut objc::runtime::Object = msg_send![
            class!(NSString),
            stringWithUTF8String: b"NSWorkspaceDidActivateApplicationNotification\0".as_ptr()
        ];

        let _: *mut objc::runtime::Object = msg_send![
            notification_center,
            addObserverForName: notification_name
            object: std::ptr::null::<objc::runtime::Object>()
            queue: std::ptr::null::<objc::runtime::Object>()
            usingBlock: &*block
        ];
    });
}

fn start_ax_focus_observer_for_current_app() {
    let Some(pid) = frontmost_application_info().pid else {
        return;
    };

    if let Some(state) = STATE.get() {
        if let Ok(mut guard) = state.lock() {
            if guard.observed_pid == Some(pid) {
                return;
            }
            guard.observed_pid = Some(pid);
        }
    }

    if let Err(error) = unsafe { install_ax_focus_observer(pid) } {
        log::debug!(
            "[ovim eligibility] failed to install AX focus observer pid={}: {}",
            pid,
            error
        );
    }
}

unsafe fn install_ax_focus_observer(pid: i32) -> Result<(), String> {
    let app_element = RetainedCf::new(AXUIElementCreateApplication(pid))
        .ok_or("AXUIElementCreateApplication returned null")?;

    let mut observer: CFTypeRef = std::ptr::null();
    let create_result = AXObserverCreate(pid, ax_focus_observer_callback, &mut observer);
    let observer = RetainedCf::new(observer)
        .ok_or_else(|| format!("AXObserverCreate failed with {}", create_result))?;
    if create_result != 0 {
        return Err(format!("AXObserverCreate failed with {}", create_result));
    }

    let notification = CFString::new("AXFocusedUIElementChanged");
    let add_result = AXObserverAddNotification(
        observer.0,
        app_element.0,
        notification.as_CFTypeRef(),
        std::ptr::null_mut(),
    );
    if add_result != 0 {
        return Err(format!(
            "AXObserverAddNotification AXFocusedUIElementChanged failed with {}",
            add_result
        ));
    }

    let run_loop_source = AXObserverGetRunLoopSource(observer.0);
    if run_loop_source.is_null() {
        return Err("AXObserverGetRunLoopSource returned null".to_string());
    }
    let mode = CFString::new("kCFRunLoopDefaultMode");
    CFRunLoopAddSource(CFRunLoopGetMain(), run_loop_source, mode.as_CFTypeRef());

    let observers = AX_OBSERVERS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = observers.lock() {
        guard.push(observer);
    }

    Ok(())
}

extern "C" fn ax_focus_observer_callback(
    _observer: CFTypeRef,
    _element: CFTypeRef,
    _notification: CFStringRef,
    _refcon: *mut c_void,
) {
    invalidate_cache(FocusCheckReason::AxFocusChanged);
    recompute(FocusCheckReason::AxFocusChanged);
}
