//! 解析远程 Linux 主机 `/proc`、`df`、`ps` 等命令输出，生成 [`SysInfo`]。
//!
//! CPU 使用率与网络速率均为「差分量」，需要保留上一次采样状态，因此提供一个
//! [`SysSampler`]，由 worker 持有并跨周期复用。

use std::collections::HashMap;

use crate::proto::{DiskInfo, GpuInfo, ProcInfo, SysInfo};

/// 采集命令：一次 SSH exec 取回所有原始数据，用 `===MARK===` 分段，便于解析。
pub const PROBE_CMD: &str = r#"
echo '===HOST==='; hostname 2>/dev/null
echo '===IP==='; hostname -I 2>/dev/null
echo '===OS==='; (. /etc/os-release 2>/dev/null; echo "$PRETTY_NAME")
echo '===UP==='; cat /proc/uptime 2>/dev/null
echo '===LOAD==='; cat /proc/loadavg 2>/dev/null
echo '===CPU==='; grep '^cpu' /proc/stat 2>/dev/null
echo '===MEM==='; grep -E 'MemTotal|MemAvailable|SwapTotal|SwapFree' /proc/meminfo 2>/dev/null
echo '===NET==='; cat /proc/net/dev 2>/dev/null
echo '===DISK==='; df -kP 2>/dev/null
echo '===SYSCONF==='; echo "$(getconf CLK_TCK 2>/dev/null) $(getconf PAGESIZE 2>/dev/null)"
echo '===PROC==='; awk '{ pid=$1; s=$0; o=index(s,"("); c=0; for(i=length(s);i>0;i--){ if(substr(s,i,1)==")"){c=i;break} } comm=substr(s,o+1,c-o-1); rest=substr(s,c+2); n=split(rest,f," "); print pid, (f[12]+f[13]), f[22], comm }' /proc/[0-9]*/stat 2>/dev/null | sort -k2 -rn | head -n 200
echo '===GPU==='
nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null
for d in /sys/class/drm/card[0-9]*/device; do [ -r "$d/gpu_busy_percent" ] || continue; v=$(cat "$d/vendor" 2>/dev/null); case "$v" in 0x1002) vn="AMD GPU";; 0x8086) vn="Intel GPU";; *) vn="GPU";; esac; busy=$(cat "$d/gpu_busy_percent" 2>/dev/null || echo 0); used=$(cat "$d/mem_info_vram_used" 2>/dev/null || echo 0); total=$(cat "$d/mem_info_vram_total" 2>/dev/null || echo 0); idx=$(basename "$(dirname "$d")" | tr -dc 0-9); echo "$idx, $vn, ${busy:-0}, $((${used:-0}/1048576)), $((${total:-0}/1048576))"; done 2>/dev/null
echo '===END==='
"#;

/// CPU 单次采样的累计 tick：(busy, total)。
type CpuTicks = (u64, u64);

/// 跨周期保留的采样状态，用于计算差分速率。
#[derive(Default)]
pub struct SysSampler {
    prev_cpu: HashMap<String, CpuTicks>,
    /// 每网卡上次累计字节 (rx, tx)
    prev_net: HashMap<String, (u64, u64)>,
    /// 每进程上次累计 CPU tick (utime+stime)，用于按采样间隔算瞬时 CPU%（与 htop 一致）
    prev_proc: HashMap<u32, u64>,
    prev_instant: Option<std::time::Instant>,
}

impl SysSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析一次完整的探测输出，更新内部状态并返回快照。
    pub fn parse(&mut self, raw: &str) -> SysInfo {
        let sections = split_sections(raw);
        let mut info = SysInfo::default();
        // 采样间隔（秒）：用于进程瞬时 CPU% 的 tick 差分；首次或过短则为 0（该帧进程 CPU 显示 0）
        let now = std::time::Instant::now();
        let dt = self
            .prev_instant
            .map(|p| (now - p).as_secs_f64())
            .filter(|&d| d > 0.05)
            .unwrap_or(0.0);

        if let Some(s) = sections.get("HOST") {
            info.hostname = s.trim().to_string();
        }
        if let Some(s) = sections.get("IP") {
            info.ip = s.split_whitespace().next().unwrap_or("").to_string();
        }
        if let Some(s) = sections.get("OS") {
            info.os = s.trim().to_string();
        }
        if let Some(s) = sections.get("UP") {
            // /proc/uptime: "12345.67 9876.54"
            if let Some(secs) = s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                info.uptime = fmt_uptime(secs as u64);
            }
        }
        if let Some(s) = sections.get("LOAD") {
            let parts: Vec<f32> = s
                .split_whitespace()
                .take(3)
                .filter_map(|v| v.parse().ok())
                .collect();
            for (i, v) in parts.into_iter().enumerate().take(3) {
                info.load[i] = v;
            }
        }
        if let Some(s) = sections.get("CPU") {
            self.parse_cpu(s, &mut info);
        }
        if let Some(s) = sections.get("MEM") {
            parse_mem(s, &mut info);
        }
        if let Some(s) = sections.get("NET") {
            self.parse_net(s, &mut info);
        }
        if let Some(s) = sections.get("DISK") {
            info.disks = parse_disk(s);
        }
        if let Some(s) = sections.get("PROC") {
            // CLK_TCK 与页大小（默认 100 ticks/s、4KiB 页）
            let (clk_tck, page_kb) = parse_sysconf(sections.get("SYSCONF").map(|s| s.as_str()));
            info.procs = self.parse_proc_delta(s, dt, info.mem_total_kb, clk_tck, page_kb);
        }
        if let Some(s) = sections.get("GPU") {
            info.gpus = parse_gpu(s);
        }

        self.prev_instant = Some(now);
        info
    }

    /// 解析进程段（每行 `pid (utime+stime)ticks rss_pages comm…`），按 tick 差分算**瞬时** CPU%
    /// （与 htop 一致，多核可超 100%）；内存% 由 rss 页数 × 页大小 / 总内存换算。返回按 CPU 降序的前若干个。
    fn parse_proc_delta(&mut self, raw: &str, dt: f64, mem_total_kb: u64, clk_tck: f64, page_kb: u64) -> Vec<ProcInfo> {
        let mut cur: HashMap<u32, u64> = HashMap::new();
        let mut out = Vec::new();
        for line in raw.lines() {
            let mut it = line.split_whitespace();
            let (Some(pid_s), Some(tick_s), Some(rss_s)) = (it.next(), it.next(), it.next()) else { continue };
            let Ok(pid) = pid_s.parse::<u32>() else { continue };
            let ticks: u64 = tick_s.parse().unwrap_or(0);
            let rss_pages: u64 = rss_s.parse().unwrap_or(0);
            let name = it.collect::<Vec<_>>().join(" ");
            // 瞬时 CPU%：Δtick / CLK_TCK = CPU 秒；/ dt 秒 × 100 = 百分比（32 核满载 → 3200%）
            let cpu = if dt > 0.0 {
                self.prev_proc.get(&pid).map_or(0.0, |&prev| {
                    (ticks.saturating_sub(prev) as f64 / clk_tck.max(1.0) / dt * 100.0) as f32
                })
            } else {
                0.0
            };
            let mem = if mem_total_kb > 0 {
                (rss_pages.saturating_mul(page_kb) as f32) / mem_total_kb as f32 * 100.0
            } else {
                0.0
            };
            cur.insert(pid, ticks);
            out.push(ProcInfo { pid, name, cpu, mem });
        }
        self.prev_proc = cur; // 仅保留本次见到的进程，自动淘汰已退出的 pid
        out.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(40);
        out
    }

    fn parse_cpu(&mut self, raw: &str, info: &mut SysInfo) {
        let mut cores: Vec<(usize, f32)> = Vec::new();
        for line in raw.lines() {
            let mut it = line.split_whitespace();
            let Some(name) = it.next() else { continue };
            if !name.starts_with("cpu") {
                continue;
            }
            let vals: Vec<u64> = it.filter_map(|v| v.parse().ok()).collect();
            if vals.len() < 4 {
                continue;
            }
            // user+nice+system+idle+iowait+irq+softirq+steal...
            let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
            let total: u64 = vals.iter().sum();
            let busy = total.saturating_sub(idle);

            let pct = if let Some((pbusy, ptotal)) = self.prev_cpu.get(name) {
                let dt = total.saturating_sub(*ptotal);
                let db = busy.saturating_sub(*pbusy);
                if dt > 0 {
                    (db as f32 / dt as f32) * 100.0
                } else {
                    0.0
                }
            } else {
                0.0
            };
            self.prev_cpu.insert(name.to_string(), (busy, total));

            if name == "cpu" {
                info.cpu_percent = pct.clamp(0.0, 100.0);
            } else if let Ok(idx) = name[3..].parse::<usize>() {
                cores.push((idx, pct.clamp(0.0, 100.0)));
            }
        }
        cores.sort_by_key(|(i, _)| *i);
        info.cpu_cores = cores.into_iter().map(|(_, p)| p).collect();
    }

    fn parse_net(&mut self, raw: &str, info: &mut SysInfo) {
        let dt = self.prev_instant.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0).max(0.001);
        let have_prev = !self.prev_net.is_empty();
        let mut cur: HashMap<String, (u64, u64)> = HashMap::new();
        let mut rx_total = 0u64;
        let mut tx_total = 0u64;
        let (mut sum_rx_bps, mut sum_tx_bps) = (0.0f64, 0.0f64);

        for line in raw.lines() {
            let Some((iface, rest)) = line.split_once(':') else { continue };
            let iface = iface.trim();
            if iface == "lo" || iface.is_empty() {
                continue;
            }
            let cols: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
            if cols.len() < 9 {
                continue;
            }
            let (rx, tx) = (cols[0], cols[8]);
            cur.insert(iface.to_string(), (rx, tx));
            rx_total += rx;
            tx_total += tx;

            if let Some((prx, ptx)) = self.prev_net.get(iface) {
                let rx_bps = rx.saturating_sub(*prx) as f64 / dt;
                let tx_bps = tx.saturating_sub(*ptx) as f64 / dt;
                sum_rx_bps += rx_bps;
                sum_tx_bps += tx_bps;
                info.nets.push(crate::proto::NetIface { name: iface.to_string(), rx_bps, tx_bps });
            } else if have_prev {
                // 新出现的网卡，本次无差分
                info.nets.push(crate::proto::NetIface { name: iface.to_string(), rx_bps: 0.0, tx_bps: 0.0 });
            }
        }
        info.nets.sort_by(|a, b| a.name.cmp(&b.name));
        info.net_rx_bps = sum_rx_bps;
        info.net_tx_bps = sum_tx_bps;
        let _ = (rx_total, tx_total);
        self.prev_net = cur;
    }
}

/// 把带 `===NAME===` 标记的原始输出切成 段名 -> 内容。
fn split_sections(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut cur: Option<String> = None;
    let mut buf = String::new();
    for line in raw.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("===").and_then(|s| s.strip_suffix("===")) {
            if let Some(k) = cur.take() {
                map.insert(k, std::mem::take(&mut buf));
            }
            if name != "END" {
                cur = Some(name.to_string());
            }
        } else if cur.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(k) = cur.take() {
        map.insert(k, buf);
    }
    map
}

fn parse_mem(raw: &str, info: &mut SysInfo) {
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        let key = it.next().unwrap_or("");
        let val: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match key {
            "MemTotal:" => mem_total = val,
            "MemAvailable:" => mem_avail = val,
            "SwapTotal:" => swap_total = val,
            "SwapFree:" => swap_free = val,
            _ => {}
        }
    }
    info.mem_total_kb = mem_total;
    info.mem_used_kb = mem_total.saturating_sub(mem_avail);
    info.swap_total_kb = swap_total;
    info.swap_used_kb = swap_total.saturating_sub(swap_free);
}

fn parse_disk(raw: &str) -> Vec<DiskInfo> {
    let mut out = Vec::new();
    for line in raw.lines().skip(1) {
        // Filesystem 1024-blocks Used Available Capacity Mounted-on
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 {
            continue;
        }
        let fs = cols[0];
        let total: u64 = cols[1].parse().unwrap_or(0);
        let used: u64 = cols[2].parse().unwrap_or(0);
        // df 的 Available 列：已扣除 ext4 等为 root 预留的块，是用户态真正可写入的空间
        let avail: u64 = cols[3].parse().unwrap_or(0);
        let mount = cols[5..].join(" ");
        // 跳过伪文件系统与系统挂载点
        let pseudo_fs = ["tmpfs", "devtmpfs", "overlay", "squashfs", "efivarfs", "ramfs"];
        let pseudo_mount = ["/sys", "/proc", "/dev", "/run", "/snap", "/boot/efi"];
        if pseudo_fs.iter().any(|p| fs.starts_with(p))
            || pseudo_mount.iter().any(|p| mount.starts_with(p))
            || total == 0
        {
            continue;
        }
        // 占用率与 df 的 Capacity 一致：used/(used+avail)，把 root 预留块排除在分母外
        let denom = used + avail;
        let percent = if denom > 0 { used as f32 / denom as f32 * 100.0 } else { 0.0 };
        out.push(DiskInfo { mount, total_kb: total, avail_kb: avail, percent });
    }
    out
}

/// 解析 `===SYSCONF===` 段："CLK_TCK PAGESIZE"（如 "100 4096"）。
/// 返回 (每秒 tick 数, 页大小 KiB)；缺失/异常回退到 (100, 4)。
fn parse_sysconf(raw: Option<&str>) -> (f64, u64) {
    let mut it = raw.unwrap_or("").split_whitespace();
    let clk = it.next().and_then(|v| v.parse::<f64>().ok()).filter(|&v| v > 0.0).unwrap_or(100.0);
    let page_bytes = it.next().and_then(|v| v.parse::<u64>().ok()).filter(|&v| v > 0).unwrap_or(4096);
    (clk, (page_bytes / 1024).max(1))
}

/// 解析 nvidia-smi CSV：index, name, util.gpu, mem.used, mem.total（均无单位）。
fn parse_gpu(raw: &str) -> Vec<GpuInfo> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if cols.len() < 5 {
            continue;
        }
        let Ok(index) = cols[0].parse::<u32>() else { continue };
        out.push(GpuInfo {
            index,
            name: cols[1].to_string(),
            util: cols[2].parse().unwrap_or(0.0),
            mem_used_mb: cols[3].parse().unwrap_or(0),
            mem_total_mb: cols[4].parse().unwrap_or(0),
        });
    }
    out
}

fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    match crate::i18n::current() {
        crate::i18n::Lang::Zh => {
            if d > 0 {
                format!("{d}天 {h}时 {m}分")
            } else if h > 0 {
                format!("{h}时 {m}分")
            } else {
                format!("{m}分")
            }
        }
        crate::i18n::Lang::En => {
            if d > 0 {
                format!("{d}d {h}h {m}m")
            } else if h > 0 {
                format!("{h}h {m}m")
            } else {
                format!("{m}m")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gpu_csv() {
        let raw = "0, NVIDIA GeForce RTX 4090, 73, 18000, 24564\n1, NVIDIA GeForce RTX 4090, 12, 2000, 24564\n";
        let g = parse_gpu(raw);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].index, 0);
        assert_eq!(g[0].util as i32, 73);
        assert_eq!(g[0].mem_used_mb, 18000);
        assert_eq!(g[1].index, 1);
        assert_eq!(g[1].mem_total_mb, 24564);
    }

    #[test]
    fn parse_gpu_empty() {
        assert!(parse_gpu("").is_empty());
        assert!(parse_gpu("\n\n").is_empty());
    }
}
