//! AirPods Battery Watch — WinIsland 插件
//!
//! 监听 BLE 广播，解析 AirPods 电量（左耳 / 右耳 / 充电盒），
//! 在 WinIsland 动态岛上显示实时电量；任一设备电量低于阈值时
//! 以高优先级提示「电量低」。
//!
//! 电量数据直接来自 AirPods 的 BLE 广播（Apple Continuity Protocol，
//! companyId = 76），不依赖 AirPodsDesktop 等第三方程序。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
    BluetoothLEScanningMode,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::DataReader;
use winisland_plugin_api::*;

// ---------------------------------------------------------------------------
// AirPods 协议常量（AppleCP）
// ---------------------------------------------------------------------------

/// Apple 的 BLE ManufacturerData companyId。
const APPLE_VENDOR_ID: u16 = 76;
/// AirPods 广播包固定 27 字节。
const AIRPODS_PACKET_SIZE: usize = 27;
/// ProximityPairing 包类型。
const PACKET_TYPE_PROXIMITY_PAIRING: u8 = 0x7;
/// 默认低电量阈值（百分比）。
const DEFAULT_LOW_THRESHOLD: u32 = 20;

// ---------------------------------------------------------------------------
// 共享状态
// ---------------------------------------------------------------------------

struct Shared {
    token: PluginToken,
    context_api: ContextApiV1,
    context_id: AtomicU64,
    last_low: AtomicBool,
    threshold: u32,
    /// 串行化 context 的 create/update/release，避免与 shutdown 并发。
    op_lock: Mutex<()>,
}

struct Instance {
    shared: Arc<Shared>,
    watcher: BluetoothLEAdvertisementWatcher,
}

// ---------------------------------------------------------------------------
// 电量解析
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BatteryState {
    left: Option<u8>,
    right: Option<u8>,
    case_box: Option<u8>,
    left_charging: bool,
    right_charging: bool,
    case_charging: bool,
}

/// 解析 27 字节 AirPods 广播包。
///
/// 字节布局（AppleCP.h，packed）：
///   [0]      packetType = 0x7
///   [1]      remainingLength = 25
///   [2]      unk1
///   [3..5]   modelId (u16 LE)
///   [5]      flags: bit5 = broadcastFrom (1=左耳广播)
///   [6]      low nibble = curr, high nibble = anot
///   [7]      low nibble = caseBox, bit4/5/6 = curr/anot/caseCharging
///   [8..]    lid / color / 加密负载
fn parse_airpods(data: &[u8]) -> Option<BatteryState> {
    if data.len() != AIRPODS_PACKET_SIZE {
        return None;
    }
    if data[0] != PACKET_TYPE_PROXIMITY_PAIRING {
        return None;
    }
    if data[1] != (AIRPODS_PACKET_SIZE as u8 - 2) {
        return None;
    }

    let broadcast_from_left = (data[5] >> 5) & 1 == 1;
    let curr = data[6] & 0x0F;
    let anot = (data[6] >> 4) & 0x0F;
    let case_box = data[7] & 0x0F;
    let curr_charging = (data[7] >> 4) & 1 == 1;
    let anot_charging = (data[7] >> 5) & 1 == 1;
    let case_charging = (data[7] >> 6) & 1 == 1;

    // 4bit 电量值 [0,10] 有效，×10 得百分比；>10 表示不可用。
    let to_pct = |v: u8| -> Option<u8> {
        if v <= 10 {
            Some(v * 10)
        } else {
            None
        }
    };

    let (left_raw, right_raw) = if broadcast_from_left {
        (curr, anot)
    } else {
        (anot, curr)
    };
    let (left_charging, right_charging) = if broadcast_from_left {
        (curr_charging, anot_charging)
    } else {
        (anot_charging, curr_charging)
    };

    Some(BatteryState {
        left: to_pct(left_raw),
        right: to_pct(right_raw),
        case_box: to_pct(case_box),
        left_charging,
        right_charging,
        case_charging,
    })
}

fn is_low(state: &BatteryState, threshold: u32) -> bool {
    [state.left, state.right, state.case_box]
        .into_iter()
        .flatten()
        .any(|pct| (pct as u32) <= threshold)
}

// ---------------------------------------------------------------------------
// Context 更新
// ---------------------------------------------------------------------------

fn build_context(state: &BatteryState, low: bool) -> ContextDataV1 {
    let fmt = |v: Option<u8>| match v {
        Some(p) => format!("{}%", p),
        None => "--".to_string(),
    };
    let left = fmt(state.left);
    let right = fmt(state.right);
    let case_box = fmt(state.case_box);
    let body = format!("左耳 {} · 右耳 {} · 充电盒 {}", left, right, case_box);

    if low {
        ContextDataV1 {
            priority: PRIORITY_HIGH,
            flags: CONTEXT_FLAG_SHOW_COMPACT,
            timeout_ms: 0,
            title: str_to_fixed("AirPods 电量低"),
            body: str_to_fixed(&body),
            compact_text: str_to_fixed("电量低"),
            ..Default::default()
        }
    } else {
        ContextDataV1 {
            priority: PRIORITY_MEDIUM,
            flags: CONTEXT_FLAG_SHOW_COMPACT,
            timeout_ms: 0,
            title: str_to_fixed("AirPods"),
            body: str_to_fixed(&body),
            compact_text: str_to_fixed(&left),
            ..Default::default()
        }
    }
}

fn update_context(shared: &Shared, state: &BatteryState) {
    let _guard = match shared.op_lock.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    let low = is_low(state, shared.threshold);
    let ctx = build_context(state, low);
    let id = shared.context_id.load(Ordering::Relaxed);

    let result = if id == INVALID_ID {
        let Some(create) = shared.context_api.create else {
            return;
        };
        let mut new_id = INVALID_ID;
        // SAFETY: token 与函数指针来自 host，指针在本调用内有效。
        let r = unsafe { create(shared.token, &ctx, &mut new_id) };
        if r.status == 0 {
            shared.context_id.store(new_id, Ordering::Relaxed);
        }
        r
    } else {
        let Some(update) = shared.context_api.update else {
            return;
        };
        // SAFETY: 同上。
        unsafe { update(shared.token, id, &ctx) }
    };

    if result.status == 0 {
        shared.last_low.store(low, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// BLE 监听
// ---------------------------------------------------------------------------

fn on_received(
    shared: Arc<Shared>,
) -> TypedEventHandler<BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementReceivedEventArgs> {
    TypedEventHandler::new(
        move |_: &Option<BluetoothLEAdvertisementWatcher>,
              args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
            let Some(args) = args else {
                return Ok(());
            };
            let adv = match args.Advertisement() {
                Ok(a) => a,
                Err(_) => return Ok(()),
            };
            let mfr = match adv.ManufacturerData() {
                Ok(m) => m,
                Err(_) => return Ok(()),
            };

            let size = match mfr.Size() {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };
            for i in 0..size {
                let item = match mfr.GetAt(i) {
                    Ok(it) => it,
                    Err(_) => continue,
                };
                let company = match item.CompanyId() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if company != APPLE_VENDOR_ID {
                    continue;
                }
                let buf = match item.Data() {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let reader = match DataReader::FromBuffer(&buf) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let mut bytes = [0u8; AIRPODS_PACKET_SIZE];
                if reader.ReadBytes(&mut bytes).is_err() {
                    continue;
                }
                if let Some(state) = parse_airpods(&bytes) {
                    update_context(&shared, &state);
                }
            }
            Ok(())
        },
    )
}

// ---------------------------------------------------------------------------
// 插件生命周期
// ---------------------------------------------------------------------------

unsafe extern "C" fn create(
    create_info: *const PluginCreateInfoV1,
    out_handle: *mut PluginHandle,
) -> PluginResultC {
    if create_info.is_null() || out_handle.is_null() {
        return PluginResultC::err("null create argument");
    }
    // SAFETY: WinIsland 提供完整的 ABI v1 create-info 结构。
    let info = unsafe { &*create_info };
    if info.struct_size < std::mem::size_of::<PluginCreateInfoV1>() as u32
        || info.abi_version != ABI_VERSION_1
        || info.host_api.is_null()
    {
        return PluginResultC::err("unsupported create info");
    }
    // SAFETY: host API 指针在进程生命周期内有效。
    let host = unsafe { &*info.host_api };
    // SAFETY: host 由 WinIsland 提供且已校验。
    let Some(context_api) = (unsafe { host.context_api() }) else {
        return PluginResultC::err("context API is unavailable");
    };
    if context_api.create.is_none() {
        return PluginResultC::err("context create is unavailable");
    }

    // 阈值可通过环境变量覆盖。
    let threshold = std::env::var("AIRPODS_LOW_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_LOW_THRESHOLD);

    let shared = Arc::new(Shared {
        token: info.plugin_token,
        context_api,
        context_id: AtomicU64::new(INVALID_ID),
        last_low: AtomicBool::new(false),
        threshold,
        op_lock: Mutex::new(()),
    });

    let watcher = match BluetoothLEAdvertisementWatcher::new() {
        Ok(w) => w,
        Err(_) => return PluginResultC::err("failed to create BLE watcher"),
    };
    if watcher
        .SetScanningMode(BluetoothLEScanningMode::Active)
        .is_err()
    {
        return PluginResultC::err("failed to set scanning mode");
    }
    let handler = on_received(shared.clone());
    if watcher.Received(&handler).is_err() {
        return PluginResultC::err("failed to register BLE handler");
    }
    if watcher.Start().is_err() {
        return PluginResultC::err("failed to start BLE watcher");
    }

    let instance = Box::new(Instance { shared, watcher });
    // SAFETY: WinIsland 持有该不透明句柄直到 destroy。
    unsafe { out_handle.write(Box::into_raw(instance).cast::<c_void>()) };
    PluginResultC::ok()
}

unsafe extern "C" fn shutdown(handle: PluginHandle) -> PluginResultC {
    if handle.is_null() {
        return PluginResultC::ok();
    }
    // SAFETY: handle 由 create 中的 Box<Instance> 创建。
    let instance = unsafe { &mut *handle.cast::<Instance>() };

    // 停止 BLE 扫描，不再产生新回调。
    let _ = instance.watcher.Stop();

    // 等待正在执行的回调结束，再释放 context。
    let _guard = match instance.shared.op_lock.lock() {
        Ok(g) => g,
        Err(_) => return PluginResultC::err("plugin lock is poisoned"),
    };
    let id = instance.shared.context_id.load(Ordering::Relaxed);
    if id != INVALID_ID {
        if let Some(release) = instance.shared.context_api.release {
            // SAFETY: 该资源属于同一 plugin token。
            let _ = unsafe { release(instance.shared.token, id) };
        }
        instance
            .shared
            .context_id
            .store(INVALID_ID, Ordering::Relaxed);
    }
    PluginResultC::ok()
}

unsafe extern "C" fn destroy(handle: PluginHandle) {
    if !handle.is_null() {
        // SAFETY: destroy 在成功 shutdown 后仅调用一次。
        unsafe { drop(Box::from_raw(handle.cast::<Instance>())) };
    }
}

static DESCRIPTOR: PluginDescriptorV1 = PluginDescriptorV1 {
    struct_size: std::mem::size_of::<PluginDescriptorV1>() as u32,
    abi_version: ABI_VERSION_1,
    capabilities: CAPABILITY_CONTEXT,
    metadata: PluginMetadataC::new(
        "airpods-battery-watch",
        "AirPods Battery Watch",
        "0.1.0",
        "C130AIR",
        "Show AirPods battery on the island and alert when low",
    ),
    create: Some(create),
    shutdown: Some(shutdown),
    destroy: Some(destroy),
};

#[no_mangle]
/// # Safety
/// WinIsland 按文档化的 ABI v1 签名调用本函数。
pub unsafe extern "C" fn winisland_plugin_entry_v1() -> *const PluginDescriptorV1 {
    &DESCRIPTOR
}
