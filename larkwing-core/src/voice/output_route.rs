//! 采集路由「自动」档(AGENT §7.5 2026-08-12):按系统**默认输出设备**是不是耳机,把
//! `voice.capture.source = "auto"` 解析成 browser / cpal。
//! 为什么看输出:AEC 的收益 = 消「自己放的声音从**扬声器**绕回麦」;耳机出声绕不回 →
//! AEC 零收益,mac 上还倒贴(开麦的 WebView 被系统套通话处理,连自家播放一起弄糊,
//! 2026-08-12 真机 A/B 实锤)。检测尽力而为,**认不出一律当扬声器**(保持 browser——
//! AEC 多开无害是既定默认,不赌)。
//! - mac:CoreAudio 传输类型——蓝牙一律当耳机(蓝牙**音箱**会被"误伤"成 cpal,代价 =
//!   它出声时无 AEC、靠唤醒确认层兜误触;换来蓝牙耳机的 HFP / 通话处理两坑全避开);
//!   内置输出看 DataSource == 'hdpn'(有线耳机插孔)。USB 耳机认不出(无形态字段),记档。
//! - Windows:MMDevice 的 FormFactor(Headphones/Headset)——系统自带形态字段,最可靠。

/// 解析采集偏好(纯函数,单测钉着):显式 browser/cpal 原样放行;auto(以及未知值,
/// 宽容当 auto)按耳机与否定——耳机 → cpal,扬声器/认不出 → browser。
pub(super) fn resolve(pref: &str, headphones: Option<bool>) -> &'static str {
    match pref {
        "browser" => "browser",
        "cpal" => "cpal",
        _ => match headphones {
            Some(true) => "cpal",
            _ => "browser",
        },
    }
}

/// 系统默认输出是不是耳机。None = 探不出来(平台不支持 / 调用失败),调用方当扬声器。
#[cfg(target_os = "macos")]
pub(super) fn default_output_is_headphones() -> Option<bool> {
    use objc2_core_audio::{
        kAudioDevicePropertyDataSource, kAudioDevicePropertyTransportType,
        kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
        kAudioDeviceTransportTypeBuiltIn, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioObjectUnknown,
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    };
    use std::ffi::c_void;
    use std::ptr::NonNull;

    /// 读一个 u32 型属性(CoreAudio 的属性都是「地址三元组 + out 缓冲」形)。
    unsafe fn get_u32(obj: AudioObjectID, selector: u32, scope: u32) -> Option<u32> {
        let addr = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut v: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                obj,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new(&mut v as *mut u32 as *mut c_void)?,
            )
        };
        (status == 0).then_some(v)
    }

    let dev = unsafe {
        get_u32(
            kAudioObjectSystemObject as AudioObjectID,
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        )
    }?;
    if dev == kAudioObjectUnknown {
        return None;
    }
    let transport = unsafe {
        get_u32(dev, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal)
    }?;
    if transport == kAudioDeviceTransportTypeBluetooth
        || transport == kAudioDeviceTransportTypeBluetoothLE
    {
        return Some(true);
    }
    if transport == kAudioDeviceTransportTypeBuiltIn {
        // 'hdpn' = 有线耳机插在耳机孔;其余(内置扬声器 'ispk' 等)= 扬声器
        let ds = unsafe {
            get_u32(dev, kAudioDevicePropertyDataSource, kAudioObjectPropertyScopeOutput)
        };
        return Some(ds == Some(u32::from_be_bytes(*b"hdpn")));
    }
    Some(false)
}

#[cfg(windows)]
pub(super) fn default_output_is_headphones() -> Option<bool> {
    // MMDevice FormFactor:3 = Headphones / 5 = Headset 算耳机;其余(Speakers / 数字口 /
    // 未知)当扬声器。COM 初始化照 desktop.rs system_volume 的姿势(MTA + 配对释放)。
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator, PKEY_AudioEndpoint_FormFactor,
    };
    use windows::Win32::System::Com::StructuredStorage::PropVariantToUInt32;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
        STGM_READ,
    };
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let out = (|| -> Option<bool> {
            let enu: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let dev = enu.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let store = dev.OpenPropertyStore(STGM_READ).ok()?;
            let v = store.GetValue(&PKEY_AudioEndpoint_FormFactor).ok()?;
            let ff = PropVariantToUInt32(&v).ok()?;
            Some(ff == 3 || ff == 5) // EndpointFormFactor::Headphones / ::Headset
        })();
        if hr.is_ok() {
            CoUninitialize();
        }
        out
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn default_output_is_headphones() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_explicit_passthrough_auto_by_headphones() {
        assert_eq!(resolve("browser", Some(true)), "browser", "显式开:耳机也不改");
        assert_eq!(resolve("cpal", Some(false)), "cpal", "显式关:扬声器也不改");
        assert_eq!(resolve("auto", Some(true)), "cpal", "自动 + 耳机 = 关(AEC 零收益)");
        assert_eq!(resolve("auto", Some(false)), "browser", "自动 + 扬声器 = 开");
        assert_eq!(resolve("auto", None), "browser", "探不出来当扬声器,不赌");
        assert_eq!(resolve("旧值", None), "browser", "未知值宽容当 auto");
    }

    /// 形态因机而异,只钉「真走一遍平台调用不崩、能给出答案形」。
    #[test]
    fn detection_does_not_crash() {
        let _ = default_output_is_headphones();
    }
}
