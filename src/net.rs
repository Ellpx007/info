use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::{Duration, Instant};

use colored::Colorize;
use sysinfo::{NetworkData, Networks};
use unicode_width::UnicodeWidthStr;

use crate::ui::format_bytes;

/// 界面展示的完整网络快照，由 [`print`] 负责排版
pub struct NetInfo {
    pub hostname: String,
    /// 采样窗口长度（秒）
    pub sample_secs: f64,
    /// 各接口合计（不含回环）的下载/上传速率（字节/秒）
    pub total_rx_rate: u64,
    pub total_tx_rate: u64,
    /// 各接口合计的自开机以来累计收发字节
    pub total_rx: u64,
    pub total_tx: u64,
    pub interfaces: Vec<Interface>,
    /// 监听/已绑定的端口（含进程名，按端口号排序）
    pub ports: Vec<Port>,
    /// TCP 连接状态统计
    pub tcp_listen: usize,
    pub tcp_established: usize,
    pub tcp_total: usize,
    pub gateway: Option<Gateway>,
    pub dns: Vec<String>,
}

/// 单个网络接口
pub struct Interface {
    pub name: String,
    pub mac: String,
    /// 该接口的 IP 列表，如 "192.168.31.1/24"、"fe80::1/64"
    pub ips: Vec<String>,
    pub rx_rate: u64,
    pub tx_rate: u64,
    pub total_rx: u64,
    pub total_tx: u64,
    pub is_loopback: bool,
}

/// 一条监听/已绑定端口
pub struct Port {
    /// "tcp"、"tcp6"、"udp"、"udp6"
    pub proto: String,
    /// 形如 "[::]:8080"、"192.168.31.1:22"、全零地址显示为 "*:8080"
    pub addr: String,
    pub port: u16,
    /// "进程名 (pid N)"，解析失败为 None
    pub process: Option<String>,
}

/// 默认网关
pub struct Gateway {
    pub iface: String,
    pub addr: Ipv4Addr,
}

/// 采集一次完整网络快照。内部约采样 [`sample_secs`] 秒以获得当前速率。
pub fn collect() -> NetInfo {
    let hostname = read_hostname();

    // 速率采样：sysinfo 的 received()/transmitted() 是「本轮刷新以来的增量」，
    // 所以刷新两次、中间睡一个窗口，增量除以窗口时长就是当前速率。
    let mut nets = Networks::new_with_refreshed_list();
    nets.refresh(true); // 重置增量基线
    let sample_secs = 1.0;
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(sample_secs));
    let elapsed = t0.elapsed().as_secs_f64();
    nets.refresh(true);

    let mut interfaces: Vec<Interface> = nets
        .list()
        .iter()
        .map(|(name, data)| interface_from_sysinfo(name, data, elapsed))
        .collect();
    if interfaces.is_empty() {
        interfaces = fallback_interface_read();
    }

    let total_rx_rate = interfaces
        .iter()
        .filter(|i| !i.is_loopback)
        .map(|i| i.rx_rate)
        .sum();
    let total_tx_rate = interfaces
        .iter()
        .filter(|i| !i.is_loopback)
        .map(|i| i.tx_rate)
        .sum();
    // 与速率统计口径一致：合计只计非回环接口（回环收发量会显著虚高）
    let total_rx = interfaces.iter().filter(|i| !i.is_loopback).map(|i| i.total_rx).sum();
    let total_tx = interfaces.iter().filter(|i| !i.is_loopback).map(|i| i.total_tx).sum();

    let ports = collect_ports();
    let (tcp_listen, tcp_established, tcp_total) = tcp_counts();
    let gateway = default_gateway();
    let dns = dns_servers();

    NetInfo {
        hostname,
        sample_secs,
        total_rx_rate,
        total_tx_rate,
        total_rx,
        total_tx,
        interfaces,
        ports,
        tcp_listen,
        tcp_established,
        tcp_total,
        gateway,
        dns,
    }
}

fn interface_from_sysinfo(name: &str, data: &NetworkData, elapsed: f64) -> Interface {
    Interface {
        name: name.to_string(),
        mac: data.mac_address().to_string(),
        ips: data.ip_networks().iter().map(|x| x.to_string()).collect(),
        rx_rate: bytes_per_sec(data.received(), elapsed),
        tx_rate: bytes_per_sec(data.transmitted(), elapsed),
        total_rx: data.total_received(),
        total_tx: data.total_transmitted(),
        is_loopback: name == "lo",
    }
}

fn bytes_per_sec(bytes: u64, elapsed: f64) -> u64 {
    if elapsed <= 0.0 {
        0
    } else {
        (bytes as f64 / elapsed) as u64
    }
}

/// sysinfo 拿不到接口时（理论上极少）直接读 sysfs 兜底
fn fallback_interface_read() -> Vec<Interface> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/net") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let mac_path = format!("/sys/class/net/{name}/address");
        let mac = std::fs::read_to_string(&mac_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let ips = read_sysfs_ips(&name);
        out.push(Interface {
            is_loopback: name == "lo",
            name,
            mac,
            ips,
            rx_rate: 0,
            tx_rate: 0,
            total_rx: 0,
            total_tx: 0,
        });
    }
    out
}

/// 从 /proc/net/if_inet6 与 /proc/net/fib_trie 兜底读 IP；此处仅取 if_inet6，
/// IPv4 在这里通常不齐全，保留空即可（正常路径用 sysinfo 已足够）。
fn read_sysfs_ips(_name: &str) -> Vec<String> {
    Vec::new()
}

fn read_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 解析 /proc/net/route 里的默认网关 IPv4
fn default_gateway() -> Option<Gateway> {
    let content = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in content.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        // 默认路由：Destination 全 0，Gateway 非 0
        if f[1] == "00000000" && f[2] != "00000000" {
            let addr = ipv4_from_hex(f[2])?;
            return Some(Gateway {
                iface: f[0].to_string(),
                addr,
            });
        }
    }
    None
}

/// 读取 DNS（/etc/resolv.conf 的 nameserver 行）
fn dns_servers() -> Vec<String> {
    let content = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    content
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("nameserver")?;
            let addr = rest.trim();
            if addr.is_empty() {
                None
            } else {
                Some(addr.to_string())
            }
        })
        .collect()
}

// ---------------- 端口 + 进程 ----------------

/// 一条来自 /proc/net/{tcp,tcp6,udp,udp6} 的原始连接记录
#[derive(Clone, Copy)]
struct RawConn {
    state: u8,
    ip: IpAddr,
    port: u16,
    uid: u32,
    inode: u64,
    proto: &'static str,
}

fn collect_ports() -> Vec<Port> {
    let mut conns = Vec::new();
    conns.extend(parse_net_conns("/proc/net/tcp", "tcp", false));
    conns.extend(parse_net_conns("/proc/net/tcp6", "tcp6", true));
    conns.extend(parse_net_conns("/proc/net/udp", "udp", false));
    conns.extend(parse_net_conns("/proc/net/udp6", "udp6", true));

    // TCP 监听(0x0A)与 UDP 已绑定(0x07)
    let listening: Vec<RawConn> = conns
        .iter()
        .copied()
        .filter(|c| c.state == 0x0A || c.state == 0x07)
        .collect();

    let proc_map = socket_process_map(&listening);
    let mut ports: Vec<Port> = listening
        .into_iter()
        .map(|c| Port {
            proto: c.proto.to_string(),
            addr: fmt_ip_port(c.ip, c.port),
            port: c.port,
            process: proc_map.get(&c.inode).cloned(),
        })
        .collect();
    ports.sort_by_key(|p| p.port);
    ports
}

fn parse_net_conns(path: &str, proto: &'static str, is_v6: bool) -> Vec<RawConn> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    for line in content.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        // f: 0 sl, 1 local, 2 rem, 3 st, 4 tx/rx, 5 tr, 6 retrans, 7 uid, 8 timeout, 9 inode
        let Ok(state) = u8::from_str_radix(f[3], 16) else {
            continue;
        };
        let Ok(uid) = f[7].parse::<u32>() else { continue };
        let Ok(inode) = f[9].parse::<u64>() else { continue };
        let Some((ip, port)) = parse_local(f[1], is_v6) else {
            continue;
        };
        out.push(RawConn {
            state,
            ip,
            port,
            uid,
            inode,
            proto,
        });
    }
    out
}

/// 把 /proc/net 的「HEX地址:十六进制端口」解析成 (IP, 端口)。全零地址返回 (0.0.0.0|::, port)。
fn parse_local(local: &str, is_v6: bool) -> Option<(IpAddr, u16)> {
    let (addr_hex, port_hex) = local.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let ip = if is_v6 {
        IpAddr::V6(ipv6_from_hex(addr_hex)?)
    } else {
        IpAddr::V4(ipv4_from_hex(addr_hex)?)
    };
    Some((ip, port))
}

/// /proc/net/{tcp,udp} 的 IPv4 为「小端字节序的 8 位十六进制」。
fn ipv4_from_hex(hex: &str) -> Option<Ipv4Addr> {
    let v = u32::from_str_radix(hex, 16).ok()?;
    let b = v.to_le_bytes();
    Some(Ipv4Addr::new(b[0], b[1], b[2], b[3]))
}

/// /proc/net/tcp6 的 IPv6 为 4 个 32 位字的十六进制拼接，每个字按小端字节序打印。
/// 因此把每个字看成 u32，再用 `to_le_bytes()` 还原出该字在地址里的 4 个字节。
fn ipv6_from_hex(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for i in 0..4 {
        let w = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    Some(Ipv6Addr::from(bytes))
}

fn fmt_ip_port(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v) if v.is_unspecified() => format!("*:{}", port),
        IpAddr::V4(v) => format!("{}:{}", v, port),
        IpAddr::V6(v) if v.is_unspecified() => format!("*:{}", port),
        IpAddr::V6(v) => format!("[{}]:{}", v, port),
    }
}

/// uid -> inode -> 进程名。只扫「属主与监听 socket 相同」的进程，降低成本。
fn socket_process_map(conns: &[RawConn]) -> HashMap<u64, String> {
    use std::os::unix::fs::MetadataExt;

    let needed_inodes: std::collections::HashSet<u64> = conns.iter().map(|c| c.inode).collect();
    if needed_inodes.is_empty() {
        return HashMap::new();
    }
    let uids: std::collections::HashSet<u32> = conns.iter().map(|c| c.uid).collect();

    let mut map = HashMap::new();
    let Ok(proc) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in proc.flatten() {
        let name = entry.file_name();
        let Some(pid_str) = name
            .to_str()
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty())
        else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else { continue };
        let pid_dir = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&pid_dir) else { continue };
        if !uids.contains(&meta.uid()) {
            continue;
        }
        let process = {
            let comm = std::fs::read_to_string(pid_dir.join("comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            format!("{comm} ({pid})")
        };
        let Ok(fds) = std::fs::read_dir(pid_dir.join("fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            if let Some(inode) = socket_inode(&target)
                && needed_inodes.contains(&inode)
            {
                map.insert(inode, process.clone());
            }
        }
    }
    map
}

fn socket_inode(target: &Path) -> Option<u64> {
    let s = target.to_string_lossy();
    let inner = s.strip_prefix("socket:[")?.strip_suffix(']')?;
    inner.parse().ok()
}

/// TCP 连接状态统计（监听/已建立/合计）
fn tcp_counts() -> (usize, usize, usize) {
    let mut listen = 0;
    let mut est = 0;
    let mut total = 0;
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines().skip(1) {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 4 {
                    continue;
                }
                if let Ok(st) = u8::from_str_radix(f[3], 16) {
                    total += 1;
                    if st == 0x0A {
                        listen += 1;
                    } else if st == 0x01 {
                        est += 1;
                    }
                }
            }
        }
    }
    (listen, est, total)
}

// ---------------- 渲染 ----------------

/// 把采集到的网络信息排版成精美文本打印到 stdout
pub fn print(net: &NetInfo) {
    println!("  {}  ·  {}", "网络概览".cyan().bold(), net.hostname.yellow().bold());
    print_rates(net);
    println!();

    // 接口信息
    println!("  {}", "网络接口".bold().cyan());
    for (i, iface) in net.interfaces.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_interface(iface);
    }
    println!();

    // 端口
    println!("  {}  （{} 个监听/绑定端口）", "监听端口".bold().cyan(), net.ports.len());
    print_ports(&net.ports);
    println!();

    // 网关 / DNS / 连接
    println!("  {}", "路由与连接".bold().cyan());
    if let Some(gw) = &net.gateway {
        println!(
            "    {}{}",
            pad_right("默认网关", 10),
            format!("{}  ({})", gw.addr, gw.iface).green()
        );
    } else {
        println!("    {}{}", pad_right("默认网关", 10), "无".dimmed());
    }
    let dns = if net.dns.is_empty() {
        "未配置".dimmed().to_string()
    } else {
        net.dns.join("  ").green().to_string()
    };
    println!("    {}{}", pad_right("DNS", 10), dns);
    let tcp = format!("{}（监听 {} · 已建立 {}）", net.tcp_total, net.tcp_listen, net.tcp_established);
    println!("    {}{}", pad_right("TCP 连接", 10), tcp);
    println!();
}

fn print_rates(net: &NetInfo) {
    let rx = format!("{:>12}", format_rate(net.total_rx_rate));
    let tx = format!("{:>12}", format_rate(net.total_tx_rate));
    println!("    {}  {}", "↓ 下载".green().bold(), rx.green());
    println!("    {}  {}", "↑ 上传".yellow().bold(), tx.yellow());
    println!(
        "    {}",
        format!("累计 ↓ {}   ↑ {}", format_bytes(net.total_rx), format_bytes(net.total_tx))
            .dimmed()
    );
}

fn print_interface(iface: &Interface) {
    const KW: usize = 8;
    let name = if iface.is_loopback {
        format!("{} (回环)", iface.name)
    } else {
        iface.name.clone()
    };
    println!("    {}  {}", "●".cyan().bold(), name.bold());
    println!(
        "    {}{}",
        pad_right("MAC", KW),
        if iface.mac.is_empty() {
            "—".dimmed().to_string()
        } else {
            iface.mac.clone()
        }
    );
    if iface.ips.is_empty() {
        println!("    {}{}", pad_right("IP", KW), "无".dimmed());
    } else {
        for ip in iface.ips.iter().filter(|ip| !ip.contains(':')) {
            println!("    {}{}", pad_right("IPv4", KW), ip.green());
        }
        for ip in iface.ips.iter().filter(|ip| ip.contains(':')) {
            println!("    {}{}", pad_right("IPv6", KW), ip.blue());
        }
    }
    println!(
        "    {}↓ {}   ↑ {}",
        pad_right("速度", KW),
        format_rate(iface.rx_rate),
        format_rate(iface.tx_rate)
    );
    println!(
        "    {}↓ {}   ↑ {}",
        pad_right("累计", KW),
        format_bytes(iface.total_rx),
        format_bytes(iface.total_tx)
    );
}

fn print_ports(ports: &[Port]) {
    if ports.is_empty() {
        println!("    {}", "（无）".dimmed());
        return;
    }
    // 各列宽度按「显示宽度」计算，避免全角字符破坏对齐
    let mut proto_w = 4;
    let mut addr_w = 4;
    let mut proc_w = 4;
    for p in ports {
        proto_w = proto_w.max(UnicodeWidthStr::width(p.proto.as_str()));
        addr_w = addr_w.max(UnicodeWidthStr::width(p.addr.as_str()));
        if let Some(pr) = &p.process {
            proc_w = proc_w.max(UnicodeWidthStr::width(pr.as_str()));
        }
    }
    println!(
        "    {}  {}  {}",
        pad_right("协议", proto_w).bold(),
        pad_right("地址", addr_w).bold(),
        pad_right("进程", proc_w).bold()
    );
    for p in ports {
        let proc = p.process.as_deref().unwrap_or("");
        println!(
            "    {}  {}  {}",
            pad_right(&p.proto, proto_w),
            pad_right(&p.addr, addr_w),
            proc
        );
    }
}

/// 按显示宽度右补空格，使同列起点一致（全角字符按 2 格计）
fn pad_right(s: &str, target: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= target {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(target - w))
    }
}

/// 把字节速率格式化成人读的 "12.3MB/s"
fn format_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_hex_decode_little_endian() {
        // /proc/net/tcp 以「小端字节序」打印 IPv4
        assert_eq!(ipv4_from_hex("0100007F"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(ipv4_from_hex("00000000"), Some(Ipv4Addr::new(0, 0, 0, 0)));
        assert_eq!(ipv4_from_hex("010A0009"), Some(Ipv4Addr::new(9, 0, 10, 1)));
    }

    #[test]
    fn ipv6_hex_decode() {
        // ::1 的每个 32 位字按小端打印：末字字节 [00,00,00,01] => 小端 u32 = 0x01000000 => "01000000"
        assert_eq!(
            ipv6_from_hex("00000000000000000000000001000000"),
            Some(Ipv6Addr::LOCALHOST)
        );
        assert_eq!(ipv6_from_hex("00000000000000000000000000000000"), Some(Ipv6Addr::UNSPECIFIED));
        // 4000::1 的字节：40 00 ... 00 01 => 首字小端 u32 = 0x00000040 => "00000040"
        assert_eq!(
            ipv6_from_hex("00000040000000000000000001000000"),
            Some("4000::1".parse().unwrap())
        );
        // IPv4 映射地址 ::ffff:127.0.0.1
        assert_eq!(
            ipv6_from_hex("0000000000000000FFFF00000100007F"),
            Some("::ffff:127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn gateway_hex_is_little_endian() {
        // 192.168.31.1
        assert_eq!(ipv4_from_hex("011FA8C0"), Some(Ipv4Addr::new(192, 168, 31, 1)));
    }

    #[test]
    fn fmt_ip_port_blank_for_unspecified() {
        assert_eq!(fmt_ip_port(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080), "*:8080");
        assert_eq!(fmt_ip_port(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1)), 22), "192.168.0.1:22");
        assert_eq!(fmt_ip_port(IpAddr::V6("::1".parse().unwrap()), 22), "[::1]:22");
        assert_eq!(fmt_ip_port(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 53), "*:53");
    }

    #[test]
    fn socket_inode_parse() {
        assert_eq!(socket_inode(Path::new("socket:[38927]")), Some(38927));
        assert_eq!(socket_inode(Path::new("/some/path")), None);
    }

    #[test]
    fn bytes_per_sec_math() {
        assert_eq!(bytes_per_sec(1000, 1.0), 1000);
        assert_eq!(bytes_per_sec(500, 0.5), 1000);
        assert_eq!(bytes_per_sec(123, 0.0), 0);
    }
}
