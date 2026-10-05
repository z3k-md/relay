//! WebView2 settings Tauri does not expose.

use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    ICoreWebView2_19, ICoreWebView2Controller,
};
use windows::core::Interface;

use super::com::{Context, Error};

/// Ask WebView2 to keep memory low (`low`), as for a hidden window, or to go
/// back to normal. It drops caches and may page memory out; the page keeps
/// running. Needs WebView2 runtime 114 or later.
pub fn set_memory_target(controller: &ICoreWebView2Controller, low: bool) -> Result<(), Error> {
    let webview = unsafe { controller.CoreWebView2() }.ctx("CoreWebView2")?;
    let webview: ICoreWebView2_19 = webview.cast().ctx("ICoreWebView2_19")?;
    let level = if low {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
    } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
    };
    unsafe { webview.SetMemoryUsageTargetLevel(level) }.ctx("SetMemoryUsageTargetLevel")
}
