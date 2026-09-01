//! 能力轴:系统快照(只读)。「电脑为什么这么卡 / 内存够不够 / C 盘还剩多少」的手脚——
//! 一次采样把 CPU / 内存 / TOP 进程 / 各卷剩余 / 开机时长打包给模型解读(fs_usage 的姊妹:
//! 只读、聚合、TOP-N,不分页)。诊断归模型、动作归别的原语(清磁盘 fs_usage+fs_trash、
//! 关自启 startup_toggle),这里绝不长出「一键加速」类任务功能(§5)。
//!
//! CPU 占用需要两次采样之间的差值(sysinfo 契约),工具内隔 600ms 采两轮 —— 秒级返回。
//! 进程按**名字聚合**再排 TOP(浏览器一家几十个进程,不聚合的 TOP 榜没法看),
//! 百分比按全机口径(单进程 cpu / 核数,对齐任务管理器的直觉)。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};
use crate::files::human_size;

/// TOP 榜条数(CPU / 内存各一榜;§4.11 方案拍板 10)。
const TOP_N: usize = 10;
/// 两次 CPU 采样的间隔(> sysinfo MINIMUM_CPU_UPDATE_INTERVAL=200ms,取宽松值求稳)。
const SAMPLE_GAP: std::time::Duration = std::time::Duration::from_millis(600);

pub(super) struct SystemStatus {
    spec: ToolSpec,
}

impl SystemStatus {
    pub(super) fn new() -> SystemStatus {
        SystemStatus {
            spec: ToolSpec {
                name: "system_status",
                description: "看这台电脑当下的状态:CPU 总占用和占用最多的程序、内存用量和\
                              占用最多的程序、每个磁盘分区还剩多少空间、开机多久了。用户说\
                              「电脑好卡 / 风扇狂转 / 内存不够 / C 盘满没满」时先用它摸底,\
                              拿到数据再解读、再建议。只是看一眼,不改任何东西;想深挖哪个\
                              文件夹占了磁盘用 fs_usage,想看开机自启的程序用 startup_list。",
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.system_status",
            },
        }
    }
}

#[async_trait]
impl Tool for SystemStatus {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        // 采样是阻塞活(两轮之间还要睡 600ms),整段丢线程池。
        tokio::task::spawn_blocking(snapshot).await?
    }
}

/// 一次完整快照(阻塞;单测在 mac 上也真跑——sysinfo 跨平台)。
fn snapshot() -> anyhow::Result<String> {
    use sysinfo::{Disks, ProcessesToUpdate, System};

    let mut sys = System::new();
    sys.refresh_cpu_usage();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    std::thread::sleep(SAMPLE_GAP);
    sys.refresh_cpu_usage();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.refresh_memory();

    let cores = sys.cpus().len().max(1);
    let mut out = String::new();

    // 开机时长 + CPU 总况
    out.push_str(&format!(
        "开机 {};CPU {} 核,总占用 {:.0}%\n",
        human_uptime(System::uptime()),
        cores,
        sys.global_cpu_usage()
    ));

    // 进程按名字聚合(浏览器一家几十个进程),CPU 归一成全机百分比
    let mut by_name: std::collections::HashMap<String, (f32, u64)> = std::collections::HashMap::new();
    for p in sys.processes().values() {
        let name = p.name().to_string_lossy().into_owned();
        let e = by_name.entry(name).or_insert((0.0, 0));
        e.0 += p.cpu_usage();
        e.1 += p.memory();
    }
    let mut cpu_top: Vec<(&String, f32)> =
        by_name.iter().map(|(n, (c, _))| (n, *c / cores as f32)).collect();
    cpu_top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let cpu_line: Vec<String> = cpu_top
        .iter()
        .take(TOP_N)
        .filter(|(_, c)| *c >= 0.5) // 全机不到半个点的不上榜(空闲机器别列一排 0%)
        .map(|(n, c)| format!("{n} {c:.0}%"))
        .collect();
    if !cpu_line.is_empty() {
        out.push_str(&format!("占 CPU 最多:{}\n", cpu_line.join("、")));
    }

    // 内存
    out.push_str(&format!(
        "内存 {} 已用 {}({:.0}%)\n",
        human_size(sys.total_memory()),
        human_size(sys.used_memory()),
        sys.used_memory() as f64 / sys.total_memory().max(1) as f64 * 100.0
    ));
    let mut mem_top: Vec<(&String, u64)> = by_name.iter().map(|(n, (_, m))| (n, *m)).collect();
    mem_top.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    let mem_line: Vec<String> = mem_top
        .iter()
        .take(TOP_N)
        .filter(|(_, m)| *m >= 50 << 20) // 50MB 以下不上榜
        .map(|(n, m)| format!("{n} {}", human_size(*m)))
        .collect();
    if !mem_line.is_empty() {
        out.push_str(&format!("占内存最多:{}\n", mem_line.join("、")));
    }

    // 各卷容量/剩余(fs_usage 管目录树,这里管卷级「C 盘还剩多少」)
    let disks = Disks::new_with_refreshed_list();
    let mut seen = std::collections::HashSet::new();
    let mut disk_lines = Vec::new();
    for d in disks.list() {
        let mount = d.mount_point().to_string_lossy().into_owned();
        // mac 会把同一卷挂出好几个视图(/、/System/Volumes/Data…),按挂载点去重即可
        if !seen.insert(mount.clone()) || d.total_space() == 0 {
            continue;
        }
        disk_lines.push(format!(
            "{mount} 共 {} 剩 {}",
            human_size(d.total_space()),
            human_size(d.available_space())
        ));
    }
    if !disk_lines.is_empty() {
        out.push_str(&format!("磁盘:{}", disk_lines.join(";")));
    }

    Ok(out.trim_end().to_string())
}

/// 秒 → 「N 天 N 小时」/「N 小时 N 分钟」/「N 分钟」。
fn human_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分钟")
    } else {
        format!("{} 分钟", m.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_formats_by_scale() {
        assert_eq!(human_uptime(30), "1 分钟", "不足一分钟按 1 分钟(别显示 0)");
        assert_eq!(human_uptime(5 * 60), "5 分钟");
        assert_eq!(human_uptime(3 * 3600 + 20 * 60), "3 小时 20 分钟");
        assert_eq!(human_uptime(2 * 86400 + 5 * 3600), "2 天 5 小时");
    }

    /// 真采样冒烟(sysinfo 跨平台,mac 开发机就能验):有 CPU/内存/磁盘三段、数字非空。
    #[test]
    fn snapshot_reports_cpu_mem_disks() {
        let s = snapshot().unwrap();
        assert!(s.contains("CPU"), "{s}");
        assert!(s.contains("内存"), "{s}");
        assert!(s.contains("磁盘:"), "{s}");
        assert!(s.contains("开机"), "{s}");
    }
}
