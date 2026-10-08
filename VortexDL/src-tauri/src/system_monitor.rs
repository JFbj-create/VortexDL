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
    let ps = "Get-CimInstance Win32_VideoController | ForEach-Object { $mb=[math]::Round($_.AdapterRAM/1MB); Write-Output ('GPUITEM||' + $_.Name + '||' + $mb + '||' + $_.DriverVersion) }";
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
        });

        Ok(json!(SystemStatus { cpu, gpu: gpu_info, memory, disks: disk_infos }))
    }).await.map_err(|e| format!("join: {}", e))?;
    status
}
