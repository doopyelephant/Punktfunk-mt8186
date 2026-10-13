//! Every setting row the pages show, as data: the key it stores under and the label it carries.
//! The pages take both from here, so a row cannot show a label this table does not hold.

/// One row of a settings page.
pub(super) struct Row {
    /// The stored key, and the name a preset pins it under.
    pub(super) key: &'static str,
    pub(super) label: &'static str,
}

macro_rules! rows {
    ($($name:ident: $key:literal, $label:literal, $page:literal, $advanced:literal;)*) => {
        $(pub(super) const $name: Row = Row {
            key: $key,
            label: $label,
        };)*
    };
}

// Name: key, label, the sidebar page it sits on, whether it waits behind Show advanced.
rows! {
    FULLSCREEN_ON_STREAM: "fullscreen_on_stream", "Start streams fullscreen", "general", false;
    AUTO_WAKE: "auto_wake", "Auto-wake on connect", "general", false;
    START_IN: "start_in", "Start in", "general", false;
    STATS: "stats_verbosity", "Statistics overlay", "general", false;
    SHOW_ADVANCED: "show_advanced", "Show advanced", "general", false;
    ADVANCED_STATS: "advanced_stats", "Advanced statistics", "general", true;
    STATS_POSITION: "hud_placement", "Statistics position", "general", true;
    STATS_SIZE: "stats_scale_pct", "Statistics size", "general", true;
    EXIT_HINT: "exit_hint", "Exit hint", "general", true;

    RESOLUTION: "resolution", "Resolution", "display", false;
    REFRESH: "refresh_hz", "Refresh rate", "display", false;
    BITRATE: "bitrate_kbps", "Bitrate (Mb/s, 0 = automatic)", "display", false;
    VIDEO_FIT: "video_fit", "Picture fit", "display", false;
    HDR: "hdr_enabled", "10-bit HDR", "display", false;
    PRESENT: "present_priority", "Prioritize", "display", false;
    SMOOTH_BUFFER: "smooth_buffer", "Smoothness buffer", "display", true;
    RENDER_SCALE: "render_scale", "Render scale", "display", true;
    CODEC: "codec", "Video codec", "display", true;
    PYROWAVE_QUALITY: "pyrowave_bpp", "PyroWave quality", "display", true;
    CHROMA: "enable_444", "Full chroma (4:4:4)", "display", true;
    TEN_BIT_SDR: "ten_bit_sdr", "10-bit SDR", "display", true;
    VSYNC: "vsync", "V-Sync", "display", true;
    VRR: "allow_vrr", "Follow variable refresh", "display", true;
    COMPOSITOR: "compositor", "Host compositor", "display", true;
    KEEP_AWAKE: "keep_awake", "Keep screen awake", "display", true;
    DECODER: "decoder", "Video decoder", "display", true;
    ADAPTER: "adapter", "GPU", "display", true;

    TOUCH: "touch_mode", "Touch input", "input", false;
    MOUSE: "mouse_mode", "Mouse input", "input", false;
    SHORTCUTS: "inhibit_shortcuts", "Capture system shortcuts", "input", false;
    INVERT_SCROLL: "invert_scroll", "Invert scroll direction", "input", false;
    QUICK_ACTIONS: "overlay_actions", "Quick actions", "quick_actions", false;

    AUDIO_CHANNELS: "audio_channels", "Audio channels", "audio", false;
    SPEAKER: "speaker_device", "Speaker", "audio", false;
    MIC: "mic_enabled", "Stream microphone", "audio", false;
    MIC_DEVICE: "mic_device", "Microphone", "audio", false;
    AUDIO_FORMAT: "audio_format", "Audio quality", "audio", true;
    KEEP_HOST_AUDIO: "keep_host_audio", "Keep host audio playing", "audio", true;
    ECHO_CANCEL: "echo_cancel", "Echo cancellation", "audio", true;

    GAMEPAD: "gamepad", "Controller type", "controllers", false;
    PAD_RUMBLE: "pad_rumble", "Controller rumble", "controllers", false;
    FORWARDING: "gamepad_forwarding", "Forward controllers", "controllers", true;
    FORWARD_PAD: "forward_pad", "Use controller", "controllers", true;
    SYSTEM_BUTTONS: "system_buttons", "Guide button", "controllers", true;
    GUIDE_GESTURE: "guide_gesture", "Hold Select for guide", "controllers", true;
    PAD_HAPTICS: "pad_haptics", "Controller haptics", "controllers", true;
    PAD_SPEAKER: "pad_speaker", "Controller speaker", "controllers", true;
}
