//! 自绘窗口拖动：窗口直接跟随鼠标坐标移动。
//! 不走系统标题栏拖动通道，因此不会触发 Win11 的 Snap 分屏提示。

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

#[tauri::command]
pub fn begin_drag(window: tauri::Window) {
    std::thread::spawn(move || {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        let (sx, sy) = (pt.x, pt.y);
        let Ok(start) = window.outer_position() else { return };
        let label = window.label().to_string();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(12));
            // 左键松开即结束
            if (unsafe { GetAsyncKeyState(0x01 /* VK_LBUTTON */) } as u16) & 0x8000 == 0 {
                break;
            }
            // 窗口被隐藏流程移出屏幕时，立即中止拖动（否则会把窗口拖回屏幕）
            if crate::visibility::is_dissolved(&label) {
                break;
            }
            let mut p2 = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p2);
            }
            let _ = window.set_position(tauri::PhysicalPosition::new(
                start.x + (p2.x - sx),
                start.y + (p2.y - sy),
            ));
        }
    });
}
