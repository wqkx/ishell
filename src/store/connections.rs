use serde::{Deserialize, Serialize};

use super::crypto::{decrypt_secret, encrypt_secret, restrict_perms, ENC_PREFIX};
use super::paths::{config_path, expand_tilde, home_dir, write_atomic};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SavedConnection {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// "password" / "key" / "agent" / "interactive"
    #[serde(default = "default_auth")]
    pub auth_kind: String,
    /// 转发本机 ssh-agent（-A）
    #[serde(default)]
    pub forward_agent: bool,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub key_path: String,
    #[serde(default)]
    pub passphrase: String,
    // —— 可选跳板机 ——
    #[serde(default)]
    pub use_jump: bool,
    #[serde(default)]
    pub jump_host: String,
    #[serde(default = "default_port")]
    pub jump_port: u16,
    #[serde(default)]
    pub jump_username: String,
    #[serde(default = "default_auth")]
    pub jump_auth_kind: String,
    #[serde(default)]
    pub jump_password: String,
    #[serde(default)]
    pub jump_key_path: String,
    #[serde(default)]
    pub jump_passphrase: String,
    // —— 组织 ——
    /// 分组（文件夹）名；空表示「未分组」
    #[serde(default)]
    pub group: String,
    /// 标签（逗号分隔，自由文本），参与搜索
    #[serde(default)]
    pub tags: String,
}

fn default_auth() -> String {
    "password".into()
}
fn default_port() -> u16 {
    22
}

// ---------- 读写 ----------

/// 读取已保存连接列表（内存中为明文密码）。
///
/// - 文件不存在 → 空列表
/// - 解析失败 → 先把原文件备份为 `connections.json.bak`，再返回空（避免被覆盖丢失）
/// - 读到旧明文密码 → 自动改写为密文（迁移），无需用户操作
pub fn load() -> Vec<SavedConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut list: Vec<SavedConnection> = match serde_json::from_str(&text) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("connections.json 解析失败：{e}，已备份为 .bak");
            let _ = std::fs::write(path.with_extension("json.bak"), &text);
            return Vec::new();
        }
    };
    // 检测是否存在旧明文（需要迁移）
    let is_plain = |s: &str| !s.is_empty() && !s.starts_with(ENC_PREFIX);
    let needs_migrate = list.iter().any(|c| {
        is_plain(&c.password)
            || is_plain(&c.passphrase)
            || is_plain(&c.jump_password)
            || is_plain(&c.jump_passphrase)
    });
    // 解密到内存明文
    for c in &mut list {
        c.password = decrypt_secret(&c.password);
        c.passphrase = decrypt_secret(&c.passphrase);
        c.jump_password = decrypt_secret(&c.jump_password);
        c.jump_passphrase = decrypt_secret(&c.jump_passphrase);
    }
    if needs_migrate {
        if let Err(e) = save(&list) {
            log::warn!("明文密码迁移加密失败，保留原文件不覆盖：{e}");
        }
    }
    list
}

/// 写回连接列表（密码/口令加密后落盘）。
/// 任一秘密字段加密失败则整次保存中止，避免静默写入明文密码。
pub fn save(list: &[SavedConnection]) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err(match crate::i18n::current() {
            crate::i18n::Lang::Zh => "无法定位配置目录".into(),
            crate::i18n::Lang::En => "Config directory unavailable".into(),
        });
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut encrypted = Vec::with_capacity(list.len());
    for c in list {
        let mut e = c.clone();
        e.password = encrypt_secret(&c.password)?;
        e.passphrase = encrypt_secret(&c.passphrase)?;
        e.jump_password = encrypt_secret(&c.jump_password)?;
        e.jump_passphrase = encrypt_secret(&c.jump_passphrase)?;
        encrypted.push(e);
    }
    let json = serde_json::to_string_pretty(&encrypted).map_err(|e| e.to_string())?;
    write_atomic(&path, &json).map_err(|e| e.to_string())?;
    restrict_perms(&path);
    Ok(())
}

// ———————————————————————— 导入 ~/.ssh/config ————————————————————————

/// 一个 Host 块的原始字段。
#[derive(Default, Clone)]
struct SshHostBlock {
    patterns: Vec<String>,
    hostname: String,
    user: String,
    port: String,
    identity: String,
    proxyjump: String,
}

/// 拆分一行为 (关键字, 值)，兼容 `Key Value` 与 `Key=Value`（含周围空格）。
fn split_kv(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
        i += 1;
    }
    let key = &line[..i];
    let rest = line[i..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    (key, rest.trim_matches('"'))
}

/// 解析 ProxyJump 值：可能是配置内别名，或 `user@host:port`。返回 (use_jump, host, port, user)。
fn parse_proxyjump(
    val: &str,
    blocks: &[SshHostBlock],
    default_user: &str,
) -> (bool, String, u16, String) {
    let v = val.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return (false, String::new(), 22, String::new());
    }
    let first = v.split(',').next().unwrap_or(v).trim(); // 多跳取第一跳
    if let Some(b) = blocks
        .iter()
        .find(|b| b.patterns.iter().any(|p| p == first))
    {
        let host = if b.hostname.is_empty() {
            first.to_string()
        } else {
            b.hostname.clone()
        };
        let user = if b.user.is_empty() {
            default_user.to_string()
        } else {
            b.user.clone()
        };
        return (true, host, b.port.parse().unwrap_or(22), user);
    }
    let (user, rest) = match first.split_once('@') {
        Some((u, r)) => (u.to_string(), r),
        None => (default_user.to_string(), first),
    };
    let (host, port) = split_host_port(rest);
    (true, host, port, user)
}

/// 从 `host` / `host:port` / `[v6]` / `[v6]:port` / 裸 `v6` 中拆出 (host, port)，端口缺省 22。
///
/// 直接 `split_once(':')` 会在 IPv6 地址内部的第一个冒号处误拆（`[2001:db8::1]:2222` →
/// host=`[2001`、端口解析失败静默回退 22），生成不可连接的跳板机配置。这里显式处理方括号
/// 形式与裸 IPv6 字面量（冒号多于一个即判为无端口的 v6）。
fn split_host_port(rest: &str) -> (String, u16) {
    // `[v6]` 或 `[v6]:port`
    if let Some(after) = rest.strip_prefix('[') {
        if let Some((h, tail)) = after.split_once(']') {
            let port = tail
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(22);
            return (h.to_string(), port);
        }
        return (rest.to_string(), 22); // 畸形（有 [ 无 ]）：整体当主机
    }
    // 无方括号且冒号多于一个 → 裸 IPv6 字面量，无端口，整体当主机
    if rest.matches(':').count() > 1 {
        return (rest.to_string(), 22);
    }
    match rest.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (rest.to_string(), 22),
    }
}

/// 解析 `~/.ssh/config`，产出可导入的连接列表（跳过通配 Host；无 IdentityFile 默认用 agent）。
pub fn import_ssh_config() -> Vec<SavedConnection> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(home.join(".ssh").join("config")) else {
        return Vec::new();
    };
    let default_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into());
    parse_ssh_config_text(&text, &default_user)
}

/// 纯解析（便于测试）：从 ssh_config 文本生成连接列表。
fn parse_ssh_config_text(text: &str, default_user: &str) -> Vec<SavedConnection> {
    let mut blocks: Vec<SshHostBlock> = Vec::new();
    let mut cur: Option<SshHostBlock> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kw, val) = split_kv(line);
        match kw.to_ascii_lowercase().as_str() {
            "host" => {
                if let Some(b) = cur.take() {
                    blocks.push(b);
                }
                cur = Some(SshHostBlock {
                    patterns: val.split_whitespace().map(|s| s.to_string()).collect(),
                    ..Default::default()
                });
            }
            other => {
                if let Some(b) = cur.as_mut() {
                    match other {
                        "hostname" => b.hostname = val.to_string(),
                        "user" => b.user = val.to_string(),
                        "port" => b.port = val.to_string(),
                        "identityfile" if b.identity.is_empty() => b.identity = val.to_string(),
                        "proxyjump" => b.proxyjump = val.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    if let Some(b) = cur.take() {
        blocks.push(b);
    }

    let mut out = Vec::new();
    for b in &blocks {
        // 取第一个非通配模式作为名称；纯通配块（如 Host *）跳过
        let Some(name) = b
            .patterns
            .iter()
            .find(|p| !p.contains('*') && !p.contains('?'))
        else {
            continue;
        };
        let (auth_kind, key_path) = if b.identity.is_empty() {
            ("agent".to_string(), String::new())
        } else {
            ("key".to_string(), expand_tilde(&b.identity))
        };
        let (use_jump, jh, jp, ju) = parse_proxyjump(&b.proxyjump, &blocks, default_user);
        out.push(SavedConnection {
            name: name.clone(),
            host: if b.hostname.is_empty() {
                name.clone()
            } else {
                b.hostname.clone()
            },
            port: b.port.parse().unwrap_or(22),
            username: if b.user.is_empty() {
                default_user.to_string()
            } else {
                b.user.clone()
            },
            auth_kind,
            forward_agent: false,
            password: String::new(),
            key_path,
            passphrase: String::new(),
            use_jump,
            jump_host: jh,
            jump_port: jp,
            jump_username: ju,
            jump_auth_kind: "agent".into(),
            jump_password: String::new(),
            jump_key_path: String::new(),
            jump_passphrase: String::new(),
            group: crate::i18n::tr("导入", "Imported").to_string(),
            tags: String::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_config_basic() {
        let cfg = "\
# comment
Host *
    ServerAliveInterval 30

Host web
    HostName 10.0.0.5
    User deploy
    Port 2222
    IdentityFile /home/me/.ssh/id_ed25519

Host db
    HostName db.internal
    ProxyJump web

Host pat-*
    User skip
";
        let list = parse_ssh_config_text(cfg, "fallback");
        // 通配块（Host *、Host pat-*）被跳过，剩 web / db
        assert_eq!(list.len(), 2);

        let web = &list[0];
        assert_eq!(web.name, "web");
        assert_eq!(web.host, "10.0.0.5");
        assert_eq!(web.username, "deploy");
        assert_eq!(web.port, 2222);
        assert_eq!(web.auth_kind, "key");
        assert_eq!(web.key_path, "/home/me/.ssh/id_ed25519");
        assert!(!web.use_jump);

        let db = &list[1];
        assert_eq!(db.name, "db");
        assert_eq!(db.host, "db.internal");
        assert_eq!(db.username, "fallback"); // 无 User → 默认用户
        assert_eq!(db.auth_kind, "agent"); // 无 IdentityFile → agent
        assert!(db.use_jump);
        // ProxyJump web → 解析为 web 的 HostName/User/Port
        assert_eq!(db.jump_host, "10.0.0.5");
        assert_eq!(db.jump_username, "deploy");
        assert_eq!(db.jump_port, 2222);
    }

    #[test]
    fn parse_proxyjump_userhostport() {
        let (uj, h, p, u) = parse_proxyjump("bastion@gw.example.com:2200", &[], "me");
        assert!(uj);
        assert_eq!(h, "gw.example.com");
        assert_eq!(p, 2200);
        assert_eq!(u, "bastion");
    }

    #[test]
    fn parse_proxyjump_ipv6() {
        // 带端口的方括号 IPv6：host 应是纯地址、端口正确解析（旧实现会拆成 host=`[2001`、端口回退 22）
        let (_, h, p, _) = parse_proxyjump("u@[2001:db8::1]:2222", &[], "me");
        assert_eq!(h, "2001:db8::1");
        assert_eq!(p, 2222);
        // 方括号无端口 → 缺省 22
        let (_, h, p, _) = parse_proxyjump("[2001:db8::1]", &[], "me");
        assert_eq!(h, "2001:db8::1");
        assert_eq!(p, 22);
        // 裸 IPv6 字面量（无端口）：整体当主机，不被内部冒号误拆
        let (_, h, p, _) = parse_proxyjump("fe80::1", &[], "me");
        assert_eq!(h, "fe80::1");
        assert_eq!(p, 22);
        // 常规 host:port 不受影响
        let (_, h, p, _) = parse_proxyjump("gw:2200", &[], "me");
        assert_eq!(h, "gw");
        assert_eq!(p, 2200);
    }
}
