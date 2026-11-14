//! 按应用音量（Per-App Volume）：只控制网易云进程的音量，不碰系统音量。
//!
//! 走 Windows Core Audio：枚举默认渲染设备的音频会话，
//! 找到属于 cloudmusic.exe 的会话，设置其 ISimpleAudioVolume。

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionEnumerator, IAudioSessionManager2, IMMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::GUID;

/// CLSID_MMDeviceEnumerator（windows crate 未生成该常量，手动定义）
const CLSID_MMDEVICE_ENUMERATOR: GUID =
    GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);

fn pid_process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(
            full.rsplit(['\\', '/'])
                .next()
                .unwrap_or("")
                .to_lowercase(),
        )
    }
}

/// 网易云的音频会话是否全部处于 Inactive（没有在出声，即暂停/停止）
/// 返回 None = 找不到网易云的会话
pub fn cloudmusic_session_inactive() -> Option<bool> {
    use windows::Win32::Media::Audio::AudioSessionStateActive;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).ok()?;
        let enum_session: IAudioSessionEnumerator = manager.GetSessionEnumerator().ok()?;
        let count = enum_session.GetCount().ok()?;

        // 网易云有两个会话：第一个=媒体会话（播放/暂停 ~1s 翻转），
        // 第二个=辅助会话（~10s OS 空闲超时才翻转）。只看第一个，即拿即准。
        let mut media_state: Option<bool> = None;
        for i in 0..count {
            let Ok(control) = enum_session.GetSession(i) else { continue };
            let Ok(ctl2) = control.cast::<windows::Win32::Media::Audio::IAudioSessionControl2>()
            else {
                continue;
            };
            let Ok(pid) = ctl2.GetProcessId() else { continue };
            let Some(name) = pid_process_name(pid) else { continue };
            if name != "cloudmusic.exe" {
                continue;
            }
            if let Ok(st) = control.GetState() {
                if media_state.is_none() {
                    media_state = Some(st == AudioSessionStateActive);
                }
            }
        }
        media_state.map(|active| !active)
    }
}
