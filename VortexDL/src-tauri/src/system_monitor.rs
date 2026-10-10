use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
pub struct CpuInfo { pub model: String, pub usage: f64, pub cores: u32 }
#[derive(Debug, Serialize, Deserialize)]
pub struct GpuInfo {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub driver: Option<String>,
    /// ★ 2026-10-10: 3D 引擎占用率 (0~100)。
    ///   之前这个结构体**根本没有 usage 字段**，前端 `status.gpu.usage` 永远 undefined
    ///   → 监控面板 GPU 占用条恒为 0.0%（用户报"gpu占用情况不显示"）。
    #[serde(skip_serializing_if = "Option::is_none")] pub usage: Option<f64>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryInfo { pub used: u64, pub total: u64, pub used_percent: f64 }
#[derive(Debug, Serialize, Deserialize)]
pub struct DiskInfo {
    pub letter: String, pub used: u64, pub total: u64, pub used_percent: f64, pub is_removable: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct SystemStatus {
    pub cpu: CpuInfo, pub gpu: Option<GpuInfo>, pub memory: MemoryInfo, pub disks: Vec<DiskInfo>,
}

#[cfg(windows)]
pub fn query_wmi_gpus() -> Vec<(String, Option<u64>, Option<String>)> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    let mut result = Vec::new();
    // ★ 2026-10-10: 显存改用**注册表里的 64 位值**。
    //   Win32_VideoController.AdapterRAM 是 32 位 DWORD，>4GB 的卡会溢出成错值
    //   （RTX 5050 8GB 实测报 4095MB 或 0），所以先从显卡类键读
    //   `HardwareInformation.qwMemorySize`（QWORD），按 DriverDesc 匹配到具体卡；
    //   匹配不上再退回 AdapterRAM。
    let ps = concat!(
        "$reg=@{};",
        "Get-ChildItem 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}' -EA SilentlyContinue | ",
        "ForEach-Object { $p=Get-ItemProperty $_.PSPath -EA SilentlyContinue; ",
        "if($p.DriverDesc -and $p.'HardwareInformation.qwMemorySize'){ $reg[$p.DriverDesc]=[math]::Round($p.'HardwareInformation.qwMemorySize'/1MB) } };",
        "Get-CimInstance Win32_VideoController | ForEach-Object { ",
        "$mb=[math]::Round($_.AdapterRAM/1MB); if($reg.ContainsKey($_.Name)){ $mb=$reg[$_.Name] }; ",
        "Write-Output ('GPUITEM||' + $_.Name + '||' + $mb + '||' + $_.DriverVersion) }"
    );
    let out = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps])
        .creation_flags(0x08000000u32).output()
    { Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(), Err(_) => return result };
    for line in out.lines() {
        let line = line.trim();
        if line.starts_with("GPUITEM||") {
            let parts: Vec<&str> = line.split("||").collect();
            if parts.len() >= 4 {
                let name = parts[1].to_string();
                let mb = parts[2].parse::<u64>().ok().filter(|&v| v > 0);
                let dr_raw = parts[3];
                let dr = if dr_raw.is_empty() { None } else {
                    let first = dr_raw.split_whitespace().next().unwrap_or("").to_string();
                    if first.is_empty() { None } else { Some(first) }
                };
                if !name.is_empty() { result.push((name, mb, dr)); }
            }
        }
    }
    result
}
#[cfg(not(windows))]
pub fn query_wmi_gpus() -> Vec<(String, Option<u64>, Option<String>)> { vec![] }

// ============================================================================
// GPU 占用率 —— PDH 性能计数器 (2026-10-10)
// ----------------------------------------------------------------------------
// 为什么用 PDH: 取 GPU 占用最省事的办法是 `Get-Counter '\GPU Engine(*)\Utilization
// Percentage'`，但那要 **每次 spawn 一个 powershell.exe**（本文件上面刚为这个原因
// 把 GPU 信息缓存了 10 分钟 —— 每 2 秒起一次 powershell 会把机器拖卡）。
// PDH 是 Windows 自带的性能数据 API: 一次打开查询句柄、之后每次只做一次内存读取，
// 不开进程、不要管理员权限。
//
// ★ 必须用 **PdhAddEnglishCounterW**：计数器路径是**本地化**的，中文系统上是
//   `\GPU 引擎(*)\利用率`；用 PdhAddCounterW 传英文路径在中文系统上直接
//   PDH_CSTATUS_NO_OBJECT，一个数都拿不到。
// ★ 取 `engtype_3D` 这一类实例（游戏/3D 负载都在这里），把各进程实例求和；
//   单看某一个 pid 会漏掉别的进程，全类型求和又会把视频解码/拷贝引擎也算进来。
// ============================================================================
#[cfg(windows)]
mod gpu_pdh {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Performance::*;

    /// PDH_FMT_NOCAP100: windows-sys 0.61 没导出这个常量，值来自 pdh.h。
    /// 不加它时 PDH 会把每个实例的百分比**截断到 100**，多引擎相加后失真。
    const PDH_FMT_NOCAP100: u32 = 0x8000;
    const COUNTER: &str = r"\GPU Engine(*engtype_3D)\Utilization Percentage";

    /// 最近一次读到的 GPU 引擎实例数。0 表示计数器在，但一个实例都没枚举到
    /// —— 这是"占用恒为 0%"的典型故障，测试要能把它抓出来。
    static LAST_INSTANCES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// (上次算出的占用, 采样时刻)。速率计数器不能连续挨着采（间隔→0 时结果恒为 0）。
    static LAST: Mutex<Option<(Option<f64>, Instant)>> = Mutex::new(None);

    pub fn last_instance_count() -> usize {
        LAST_INSTANCES.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// PDH 句柄是裸指针，但 PDH 的这几个调用本身是线程安全的。
    struct Handle(*mut core::ffi::c_void, *mut core::ffi::c_void);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    fn handle() -> Option<&'static Handle> {
        static Q: OnceLock<Option<Handle>> = OnceLock::new();
        Q.get_or_init(|| unsafe {
            let mut hq: PDH_HQUERY = std::ptr::null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut hq) != 0 { return None; }
            let path: Vec<u16> = COUNTER.encode_utf16().chain(std::iter::once(0)).collect();
            let mut hc: PDH_HCOUNTER = std::ptr::null_mut();
            if PdhAddEnglishCounterW(hq, path.as_ptr(), 0, &mut hc) != 0 {
                PdhCloseQuery(hq);
                return None;
            }
            // 先采一次。占用率是"区间速率"，要两次采样才有值 —— 所以下面
            // usage_3d() 第一次调用会因为"间隔太短"直接返回 None，第二次才准。
            PdhCollectQueryData(hq);
            if let Ok(mut g) = LAST.lock() { *g = Some((None, Instant::now())); }
            Some(Handle(hq, hc))
        }).as_ref()
    }

    /// 3D 引擎总占用率 (0~100)。系统取不到就返回 None —— 上层会显示"未知"，
    /// **不要**拿 0.0 冒充（那就是用户看到的"占用不显示"）。
    ///
    /// ★ 采样节流（必须）: PDH 的"Utilization Percentage"是**速率计数器**，值 =
    ///   两次 CollectQueryData 之间的占用比例。如果两次采样挨得太近（比如刚
    ///   open 完立刻又 collect），间隔趋近 0 → 算出来恒为 0.0%。
    ///   所以这里 800ms 内复用上一次的结果；还没算出过有效值时返回 None。
    pub fn usage_3d() -> Option<f64> {
        let h = handle()?;
        let now = Instant::now();
        if let Ok(g) = LAST.lock() {
            if let Some((v, t)) = *g {
                if now.duration_since(t) < Duration::from_millis(800) {
                    return v;   // 上一次的结果（可能是 None = 还没算出来）
                }
            }
        }
        let v = unsafe {
            if PdhCollectQueryData(h.0) != 0 { None } else {
                let fmt = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
                let mut size: u32 = 0;
                let mut count: u32 = 0;
                // 第一次传空 buffer 只为拿所需大小
                let r = PdhGetFormattedCounterArrayW(h.1, fmt, &mut size, &mut count, std::ptr::null_mut());
                if r != PDH_MORE_DATA { None } else {
                    LAST_INSTANCES.store(count as usize, std::sync::atomic::Ordering::Relaxed);
                    if size == 0 || count == 0 { Some(0.0) } else {
                        let mut buf: Vec<u8> = vec![0u8; size as usize];
                        let mut size2 = size;
                        let mut count2 = count;
                        let r2 = PdhGetFormattedCounterArrayW(
                            h.1, fmt, &mut size2, &mut count2,
                            buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W,
                        );
                        if r2 != 0 { None } else {
                            let items = std::slice::from_raw_parts(
                                buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                                count2 as usize,
                            );
                            let mut sum = 0.0f64;
                            for it in items {
                                // CStatus != 0 的实例是"这一拍还没算出值"（新起的进程），跳过
                                if it.FmtValue.CStatus == 0 {
                                    sum += it.FmtValue.Anonymous.doubleValue;
                                }
                            }
                            Some(sum.clamp(0.0, 100.0))
                        }
                    }
                }
            }
        };
        if let Ok(mut g) = LAST.lock() { *g = Some((v, now)); }
        v
    }
}

#[cfg(not(windows))]
mod gpu_pdh {
    pub fn usage_3d() -> Option<f64> { None }
    pub fn last_instance_count() -> usize { 0 }
}

// ★ 性能修复 (2026-09-08): 缓存 GPU 信息 (10 分钟), 避免每次都 spawn powershell 进程
//   原代码每 2 秒调一次 query_wmi_gpus → 每次启动 powershell.exe → 严重卡顿/崩溃
static GPU_CACHE: Mutex<Option<(Vec<(String, Option<u64>, Option<String>)>, Instant)>> = Mutex::new(None);
const GPU_CACHE_TTL: Duration = Duration::from_secs(600);

fn cached_gpus() -> Vec<(String, Option<u64>, Option<String>)> {
    if let Ok(Some((cached, ts))) = GPU_CACHE.lock().map(|c| c.clone()) {
        if ts.elapsed() < GPU_CACHE_TTL {
            return cached;
        }
    }
    let fresh = query_wmi_gpus();
    if let Ok(mut c) = GPU_CACHE.lock() {
        *c = Some((fresh.clone(), Instant::now()));
    }
    fresh
}

/// ★ 性能修复 (2026-09-08): 把 sysinfo + WMI 的同步重活挪到 spawn_blocking,
///   避免阻塞 tokio 异步 executor (此前 std::thread::sleep(150ms) 直接卡住整个 runtime,
///   导致 download 轮询/搜索 invoke 全部排队 → UI 卡顿/崩溃).
///   CPU 使用率采样改为读取两次采样的差值 (sysinfo 标准 API 用法),
///   用 50ms 间隔足够; 不再每 2s 全量重建 System.
pub async fn get_system_status() -> Result<Value, String> {
    let status = tokio::task::spawn_blocking(|| -> Result<Value, String> {
        use sysinfo::{Disks, System};
        let mut sys = System::new();
        sys.refresh_cpu();
        // 首次采样后等待一小段时间再取一次, 得到非零的 cpu_usage
        std::thread::sleep(std::time::Duration::from_millis(50));
        sys.refresh_cpu();
        sys.refresh_memory();
        let cpus = sys.cpus();
        let cores: u32 = cpus.len() as u32;
        let (brand, vendor, freq) = if cores > 0 {
            let c0 = &cpus[0];
            (c0.brand().to_string(), c0.vendor_id().to_string(), c0.frequency())
        } else { (String::new(), String::new(), 0) };
        let cpu_model = if !brand.is_empty() { brand } else if cores > 0 {
            cpus.iter().next().map(|c| c.name().to_string()).unwrap_or_default()
        } else { "Unknown CPU".to_string() };
        let cpu_model = if cpu_model.trim().is_empty() {
            if !vendor.is_empty() { format!("{} CPU @ {}MHz", vendor, freq) } else { "Unknown CPU".to_string() }
        } else { cpu_model };
        let usage_sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
        let global_usage: f64 = if cores > 0 { (usage_sum as f64 / cores as f64).clamp(0.0, 100.0) } else { 0.0 };
        let cpu = CpuInfo { model: cpu_model, usage: global_usage, cores: cores.max(1) };

        let total_mem = sys.total_memory();
        let used_mem = sys.used_memory();
        let used_pct = if total_mem > 0 { (used_mem as f64 / total_mem as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 };
        let memory = MemoryInfo { used: used_mem, total: total_mem, used_percent: used_pct };

        let disks = Disks::new_with_refreshed_list();
        let mut disk_infos: Vec<DiskInfo> = Vec::new();
        for d in disks.list() {
            let mount = d.mount_point();
            let mount_str = mount.to_string_lossy().to_string();
            let letter: String = if mount_str.len() >= 2 && mount_str.as_bytes().get(1) == Some(&b':') {
                mount_str[0..1].to_uppercase()
            } else {
                #[cfg(windows)] { continue; }
                #[cfg(not(windows))] { mount_str.clone() }
            };
            if disk_infos.iter().any(|x| x.letter == letter) { continue; }
            let total_bytes = d.total_space();
            let avail_bytes = d.available_space();
            if total_bytes == 0 { continue; }
            let used_bytes = total_bytes.saturating_sub(avail_bytes);
            let pct = (used_bytes as f64 / total_bytes as f64 * 100.0).clamp(0.0, 100.0);
            let removable = letter == "A" || letter == "B";
            disk_infos.push(DiskInfo { letter, used: used_bytes, total: total_bytes, used_percent: pct, is_removable: removable });
        }
        disk_infos.sort_by(|a, b| a.letter.cmp(&b.letter));

        // GPU 用缓存 (10 分钟 TTL), 不每次 spawn powershell
        let mut gpus = cached_gpus();
        gpus.sort_by(|a, b| {
            let a_virtual = a.0.to_lowercase().contains("todesk") || a.0.to_lowercase().contains("virtual") || a.0.to_lowercase().contains("basic display") || a.0.to_lowercase().contains("microsoft");
            let b_virtual = b.0.to_lowercase().contains("todesk") || b.0.to_lowercase().contains("virtual") || b.0.to_lowercase().contains("basic display") || b.0.to_lowercase().contains("microsoft");
            a_virtual.cmp(&b_virtual)
        });
        let gpu_info = gpus.into_iter().next().map(|(name, mb, dr)| GpuInfo {
            model: if name.is_empty() { "未检测到".to_string() } else { name },
            memory_mb: mb, driver: dr,
            usage: gpu_pdh::usage_3d(),
        });

        Ok(json!(SystemStatus { cpu, gpu: gpu_info, memory, disks: disk_infos }))
    }).await.map_err(|e| format!("join: {}", e))?;
    status
}

#[cfg(all(test, windows))]
mod gpu_pdh_tests {
    /// 真读一次 GPU 3D 占用率。
    /// 断言 `is_some()` 是关键 —— 用户报的就是"占用不显示"，
    /// 如果 PDH 计数器拿不到，这个测试必须红，不能静默变成 0%。
    /// 另外断言**枚举到了引擎实例** —— 计数器在、但实例数为 0 时占用恒为 0%，
    /// 那正是"看着像没显示"的第二种坏法。
    #[test]
    #[ignore]
    fn live_gpu_usage_readable() {
        // ★ 第一次调用**按设计**返回 None：PDH 的占用率是两个采样点之间的速率，
        //   open 完立刻再采一次间隔≈0，算出来必然是 0。真实调用来自几秒一次的状态
        //   轮询，所以这里睡 1.5 秒再采第二次，模拟真实节奏。
        let first = super::gpu_pdh::usage_3d();
        println!("[GPU] 第一次(间隔太短, 允许 None) = {:?}", first);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let v = super::gpu_pdh::usage_3d();
        let n = super::gpu_pdh::last_instance_count();
        println!("[GPU] 第二次 usage_3d = {:?}, 引擎实例 = {}", v, n);
        let u = v.expect("PDH 读不到 GPU 占用 (计数器路径/英文计数器 API/实例枚举都可能是原因)");
        assert!((0.0..=100.0).contains(&u), "占用率越界: {u}");
        assert!(n > 0, "GPU 引擎实例数为 0 —— 计数器枚举失败, 占用会恒为 0%");
        println!("[GPU] OK, 3D 引擎占用 {u:.1}% / {n} 个实例");
    }

    /// 显存必须 > 4GB 量级（RTX 5050 是 8GB）。
    /// ★ 旧实现用 Win32_VideoController.AdapterRAM（32 位 DWORD）实测报 **4095MB**，
    ///   这里断言读到的不是那个溢出值。
    #[test]
    #[ignore]
    fn live_gpu_vram_not_truncated() {
        let gpus = super::query_wmi_gpus();
        println!("[GPU] wmi = {:?}", gpus);
        assert!(!gpus.is_empty(), "一个显卡都没枚举到");
        let mb = gpus[0].1.expect("显存读不到");
        assert!(mb != 4095 && mb > 4096, "显存疑似仍是 32 位溢出的 {mb}MB");
        println!("[GPU] OK, 显存 {mb}MB");
    }
}
