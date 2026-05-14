//! Keyboard event handler for vim mode processing

mod click_mode;
pub mod double_tap;
mod list_mode;
mod scroll_mode;
mod shortcuts;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::abbreviations::SharedAbbreviationState;
use crate::click_mode::SharedClickModeManager;
use crate::commands::RecordedKey;
use crate::config::click_mode::DoubleTapModifier;
use crate::config::Settings;
use crate::keyboard::{KeyCode, KeyEvent};
use crate::list_mode::SharedListModeState;
use crate::nvim_edit::EditSessionManager;
use crate::scroll_mode::SharedScrollModeState;
use crate::vim::{VimMode, VimState};

use click_mode::handle_click_mode_key;
use double_tap::{DoubleTapKey, DoubleTapManager};
use list_mode::handle_list_mode_key;
use scroll_mode::handle_scroll_mode_key;
use shortcuts::{
    check_click_mode_shortcut, check_nvim_edit_shortcut, check_vim_key,
    is_scroll_mode_enabled_for_app, process_vim_input,
};

/// Callback type for when a double-tap triggers a mode activation
pub type DoubleTapCallback = Box<dyn Fn(DoubleTapKey) + Send + 'static>;

const JK_NORMAL_MODE_WINDOW: Duration = Duration::from_millis(100);

#[derive(Default)]
struct JkNormalModeState {
    last_j_down: Option<Instant>,
}

/// Create the keyboard callback that processes key events
pub fn create_keyboard_callback(
    vim_state: Arc<Mutex<VimState>>,
    settings: Arc<Mutex<Settings>>,
    record_key_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<RecordedKey>>>>,
    edit_session_manager: Arc<EditSessionManager>,
    click_mode_manager: SharedClickModeManager,
    double_tap_manager: Arc<Mutex<DoubleTapManager>>,
    double_tap_callback: DoubleTapCallback,
    scroll_state: SharedScrollModeState,
    list_state: SharedListModeState,
    abbreviation_state: SharedAbbreviationState,
) -> impl Fn(KeyEvent) -> Option<KeyEvent> + Send + 'static {
    let jk_normal_mode_state = Arc::new(Mutex::new(JkNormalModeState::default()));

    move |event| {
        // Reset modifier double-tap trackers when any non-modifier key is pressed.
        // This prevents false double-tap detection when using shortcuts like CMD+C
        // followed quickly by CMD+V (which would otherwise look like two CMD taps).
        if event.is_key_down {
            if let Some(keycode) = event.keycode() {
                match keycode {
                    KeyCode::Escape => {}
                    _ => {
                        let mut dt_manager = double_tap_manager.lock().unwrap();
                        dt_manager.command_tracker.reset();
                        dt_manager.option_tracker.reset();
                        dt_manager.control_tracker.reset();
                        dt_manager.shift_tracker.reset();
                    }
                }
            }
        }

        // Check for Escape key double-tap (for non-modifier double-tap shortcuts)
        if let Some(keycode) = event.keycode() {
            if keycode == KeyCode::Escape {
                let mut dt_manager = double_tap_manager.lock().unwrap();
                if let Some(double_tap_key) = dt_manager.process_key_event(DoubleTapKey::Escape, event.is_key_down) {
                    // Check if Escape double-tap is configured for either mode
                    let settings_guard = settings.lock().unwrap();
                    let click_uses_escape = settings_guard.click_mode.double_tap_modifier == DoubleTapModifier::Escape;
                    let nvim_uses_escape = settings_guard.nvim_edit.double_tap_modifier == DoubleTapModifier::Escape;
                    drop(settings_guard);

                    if click_uses_escape || nvim_uses_escape {
                                crate::nvim_edit::vim_eligibility::allows_vim_at_decision_point(
                                    crate::nvim_edit::vim_eligibility::FocusCheckReason::PreNormalModeCheck,
                                );
                        double_tap_callback(double_tap_key);
                        return None; // Suppress the escape key
                    }
                }
            }
        }
        // Check if click mode is active - if so, route keys there first
        {
            let click_manager = click_mode_manager.lock().unwrap();
            if click_manager.is_active() {
                drop(click_manager);
                return handle_click_mode_key(event, Arc::clone(&click_mode_manager));
            }
        }

        // Check if we're recording a key (only on key down)
        if event.is_key_down {
            if let Some(recorded) = try_record_key(&event, &record_key_tx) {
                let mut record_tx = record_key_tx.lock().unwrap();
                if let Some(tx) = record_tx.take() {
                    let _ = tx.send(recorded);
                    return None;
                }
            }
        }

        if let Some(result) = handle_jk_normal_mode_chord(
            &event,
            Arc::clone(&vim_state),
            Arc::clone(&settings),
            Arc::clone(&jk_normal_mode_state),
        ) {
            return result;
        }

        // Check shortcuts on key down
        if event.is_key_down {
            let settings_guard = settings.lock().unwrap();

            // Check nvim edit shortcut
            if let Some(result) = check_nvim_edit_shortcut(
                &event,
                &settings_guard,
                Arc::clone(&edit_session_manager),
                Arc::clone(&settings),
            ) {
                return result;
            }

            // Check click mode shortcut
            if let Some(result) = check_click_mode_shortcut(
                &event,
                &settings_guard,
                Arc::clone(&click_mode_manager),
            ) {
                return result;
            }

            // Check vim key
            if let Some(result) = check_vim_key(&event, &settings_guard, Arc::clone(&vim_state)) {
                return result;
            }
        }

        // Check list mode first - process if:
        // 1. List navigation is enabled in scroll_mode settings
        // 2. App is in list_navigation_apps list (or enabled_apps if list_navigation_apps is empty)
        // 3. No overlay window from blocklisted apps is visible
        // 4. No text field is currently focused
        // 5. Vim mode is in Insert mode OR vim is disabled for this app
        {
            let settings_guard = settings.lock().unwrap();
            let scroll_settings = &settings_guard.scroll_mode;

            if scroll_settings.enabled && scroll_settings.list_navigation {
                // Use list_navigation_apps if non-empty, otherwise check enabled_apps
                let list_apps = if !scroll_settings.list_navigation_apps.is_empty() {
                    &scroll_settings.list_navigation_apps
                } else {
                    &scroll_settings.enabled_apps
                };
                let app_enabled = is_scroll_mode_enabled_for_app(list_apps);

                if app_enabled {
                    // Skip list mode if an overlay from a blocklisted app is visible
                    if crate::nvim_edit::accessibility::has_visible_overlay_window(&scroll_settings.overlay_blocklist) {
                        // Overlay window visible, don't intercept keys
                    } else if crate::nvim_edit::accessibility::is_text_field_focused() {
                        // Text field is focused, don't intercept hjkl for navigation
                    } else {
                        let vim_mode = vim_state.lock().unwrap().mode();
                        let vim_disabled_for_app =
                            settings_guard.ignored_apps.iter().any(|app| {
                                #[cfg(target_os = "macos")]
                                {
                                    if let Some(bundle_id) = get_frontmost_app_bundle_id() {
                                        return app == &bundle_id;
                                    }
                                }
                                false
                            });

                        // Only process list mode if vim is in Insert mode or vim is disabled for this app
                        if vim_mode == VimMode::Insert || vim_disabled_for_app || !settings_guard.enabled
                        {
                            drop(settings_guard);

                            // Process list mode key
                            let result = handle_list_mode_key(event, &list_state);

                            // If list mode handled the key, return the result
                            if result.is_none() {
                                return None;
                            }
                            // Otherwise continue to scroll/vim processing
                        }
                    }
                }
            }
        }

        // Check scroll mode - process if:
        // 1. Scroll mode is enabled
        // 2. App is in enabled_apps list
        // 3. No overlay window from blocklisted apps is visible
        // 4. No text field is currently focused
        // 5. Vim mode is in Insert mode (so scroll mode doesn't interfere with vim Normal mode)
        //    OR vim mode is disabled for this app
        {
            let settings_guard = settings.lock().unwrap();
            let scroll_settings = &settings_guard.scroll_mode;

            if scroll_settings.enabled {
                let app_enabled = is_scroll_mode_enabled_for_app(&scroll_settings.enabled_apps);

                if app_enabled {
                    // Skip scroll mode if an overlay from a blocklisted app is visible
                    if crate::nvim_edit::accessibility::has_visible_overlay_window(&scroll_settings.overlay_blocklist) {
                        // Overlay window visible, don't intercept keys
                    } else if crate::nvim_edit::accessibility::is_text_field_focused() {
                        // Text field is focused, don't intercept hjkl for scrolling
                    } else {
                        let vim_mode = vim_state.lock().unwrap().mode();
                        let vim_disabled_for_app =
                            settings_guard.ignored_apps.iter().any(|app| {
                                #[cfg(target_os = "macos")]
                                {
                                    if let Some(bundle_id) = get_frontmost_app_bundle_id() {
                                        return app == &bundle_id;
                                    }
                                }
                                false
                            });

                        // Only process scroll mode if vim is in Insert mode or vim is disabled for this app
                        if vim_mode == VimMode::Insert || vim_disabled_for_app || !settings_guard.enabled
                        {
                            let scroll_step = scroll_settings.scroll_step;
                            let disabled_shortcuts = scroll_settings.disabled_shortcuts.clone();
                            drop(settings_guard);

                            // Process scroll mode key
                            let result = handle_scroll_mode_key(
                                event,
                                &scroll_state,
                                scroll_step,
                                &disabled_shortcuts,
                            );

                            // If scroll mode handled the key, return the result
                            if result.is_none() {
                                return None;
                            }
                            // Otherwise continue to vim/abbreviation processing below.
                        }
                    }
                }
            }
        }

        // Process normal vim input
        let result = process_vim_input(event, &settings, &vim_state);
        if result.is_some() {
            handle_insert_mode_abbreviations(
                &event,
                Arc::clone(&settings),
                Arc::clone(&vim_state),
                Arc::clone(&abbreviation_state),
            );
        }
        result
    }
}

fn handle_insert_mode_abbreviations(
    event: &KeyEvent,
    settings: Arc<Mutex<Settings>>,
    vim_state: Arc<Mutex<VimState>>,
    abbreviation_state: SharedAbbreviationState,
) {
    let should_process = settings.lock().map(|settings| settings.enabled).unwrap_or(false)
        && vim_state.lock().map(|state| state.mode()).unwrap_or(VimMode::Insert) == VimMode::Insert;

    if should_process {
        abbreviation_state.lock().map(|mut state| state.process_key(event)).ok();
    }
}

fn handle_jk_normal_mode_chord(
    event: &KeyEvent,
    vim_state: Arc<Mutex<VimState>>,
    settings: Arc<Mutex<Settings>>,
    jk_state: Arc<Mutex<JkNormalModeState>>,
) -> Option<Option<KeyEvent>> {
    if !event.is_key_down || has_modifiers(event) {
        return None;
    }

    if !settings.lock().map(|settings| settings.enabled).unwrap_or(false) {
        return None;
    }

    if vim_state.lock().map(|state| state.mode()).unwrap_or(VimMode::Insert) != VimMode::Insert {
        jk_state.lock().map(|mut state| state.last_j_down = None).ok();
        return None;
    }

    match event.keycode()? {
        KeyCode::J => {
            jk_state.lock().map(|mut state| state.last_j_down = Some(Instant::now())).ok();
            None
        }
        KeyCode::K => {
            let saw_recent_j = jk_state
                .lock()
                .map(|mut state| {
                    let saw_recent_j = state
                        .last_j_down
                        .is_some_and(|timestamp| timestamp.elapsed() <= JK_NORMAL_MODE_WINDOW);
                    state.last_j_down = None;
                    saw_recent_j
                })
                .unwrap_or(false);

            if !saw_recent_j {
                return None;
            }

            if !crate::nvim_edit::vim_eligibility::allows_vim_at_decision_point(
                crate::nvim_edit::vim_eligibility::FocusCheckReason::PreNormalModeCheck,
            ) {
                return None;
            }

            // The `j` key has already reached the focused text field. Remove it
            // before entering simulated normal mode so `jk` behaves like a chord.
            if let Err(e) = crate::keyboard::inject_key_press(
                KeyCode::Delete,
                crate::keyboard::Modifiers::default(),
            ) {
                log::warn!("Failed to delete jk chord prefix before normal mode: {}", e);
            }

            {
                let mut state = vim_state.lock().unwrap();
                state.set_mode_external(VimMode::Normal);
            }
            if let Some(app) = crate::get_app_handle() {
                use tauri::Emitter;
                let _ = app.emit("mode-change", "normal");
            }

            Some(None)
        }
        _ => {
            jk_state.lock().map(|mut state| state.last_j_down = None).ok();
            None
        }
    }
}

fn has_modifiers(event: &KeyEvent) -> bool {
    event.modifiers.shift
        || event.modifiers.control
        || event.modifiers.command
}

/// Get the bundle identifier of the frontmost application
#[cfg(target_os = "macos")]
fn get_frontmost_app_bundle_id() -> Option<String> {
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let workspace: *mut objc::runtime::Object =
            msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut objc::runtime::Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let bundle_id: *mut objc::runtime::Object = msg_send![app, bundleIdentifier];
        if bundle_id.is_null() {
            return None;
        }
        let utf8: *const std::os::raw::c_char = msg_send![bundle_id, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Try to record a key if recording is active
fn try_record_key(
    event: &KeyEvent,
    record_key_tx: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<RecordedKey>>>>,
) -> Option<RecordedKey> {
    use crate::commands::RecordedModifiers;

    let record_tx = record_key_tx.lock().unwrap();
    if record_tx.is_some() {
        if let Some(keycode) = event.keycode() {
            return Some(RecordedKey {
                name: keycode.to_name().to_string(),
                display_name: keycode.to_display_name().to_string(),
                modifiers: RecordedModifiers {
                    shift: event.modifiers.shift,
                    control: event.modifiers.control,
                    option: event.modifiers.option,
                    command: event.modifiers.command,
                },
            });
        }
    }
    None
}
