//! 把「这次 MCP 连接是从哪个终端里拉起的」对回 GUI 会话，不改线协议。
//!
//! 代理沿进程祖先找到的登录 shell，本来就写在会话文件里，但握手之后 GUI 只看得到
//! `actor`（哪个 AI 客户端），对不上标签。这里补的是进程内的对应关系：
//!
//! - 本机：接受连接时用 `SO_PEERCRED` 拿到 `ishell-mcp` 的 pid，再沿父进程找到已登记的 shell。
//! - SSH：反向转发的桥跑在本进程里，连上来之前先写一行 `{"ishell_parent":<uid>}`。
//!   只有对端 pid 是本进程时才认这一行，本地代理伪造不了。
//!
//! 会话文件格式和业务 JSON 都不变，旧版 `ishell-mcp` 照常能连。

use std::collections::HashMap;
use std::sync::Mutex;

/// `(shell pid → 会话 uid)`，带一代号：重连时旧 worker 的 drop 不能把新 shell 的登记清掉。
struct Binding {
    uid: u64,
    gen: u64,
}

static LOCAL_SHELLS: Mutex<Option<HashMap<u32, Binding>>> = Mutex::new(None);
static NEXT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn map() -> std::sync::MutexGuard<'static, Option<HashMap<u32, Binding>>> {
    LOCAL_SHELLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 本机登录 shell 登记成功后调用。返回的代号交给 [`unbind_local`]。
pub fn bind_local(shell_pid: u32, session_uid: u64) -> u64 {
    if shell_pid == 0 {
        return 0;
    }
    let gen = NEXT_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    map().get_or_insert_with(HashMap::new).insert(
        shell_pid,
        Binding {
            uid: session_uid,
            gen,
        },
    );
    gen
}

/// 只撤掉这一代登记。pid 已被新 shell 复用时不动新的那条。
pub fn unbind_local(shell_pid: u32, gen: u64) {
    if gen == 0 {
        return;
    }
    let mut guard = map();
    let Some(slots) = guard.as_mut() else {
        return;
    };
    if slots.get(&shell_pid).is_some_and(|b| b.gen == gen) {
        slots.remove(&shell_pid);
    }
}

fn lookup(shell_pid: u32) -> Option<u64> {
    map()
        .as_ref()
        .and_then(|m| m.get(&shell_pid).map(|b| b.uid))
}

/// 从 `start` 起（含自身）沿父进程找第一个已登记的本机 shell。
fn ancestor_session(start: u32, ppid_of: &dyn Fn(u32) -> Option<u32>) -> Option<u64> {
    let mut pid = start;
    let mut seen = [0u32; 64];
    for i in 0..64 {
        if pid <= 1 || seen[..i].contains(&pid) {
            return None;
        }
        seen[i] = pid;
        if let Some(uid) = lookup(pid) {
            return Some(uid);
        }
        pid = ppid_of(pid)?;
    }
    None
}

/// 本机直连的 `ishell-mcp`：沿真实进程树找它的登录 shell。
#[cfg(unix)]
pub fn parent_of_peer(start: u32) -> Option<u64> {
    ancestor_session(start, &|pid| {
        crate::mcp_protocol::proc_info(pid).map(|info| info.ppid)
    })
}

/// 对端进程 pid。内核对这个 fd 的报告，调用方改不了。
#[cfg(unix)]
pub fn peer_pid(fd: std::os::unix::io::RawFd) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY：cred 是 getsockopt 要求的输出缓冲，长度如实传入。
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY：同上，输出缓冲是一个 pid_t。
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                &mut pid as *mut libc::pid_t as *mut libc::c_void,
                &mut len,
            )
        };
        (rc == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
}

/// SSH 桥在拷贝代理字节之前写的一行。不是业务协议的一部分：监听端只在对端是本进程时才读它。
pub fn parent_preamble(session_uid: u64) -> String {
    format!("{{\"ishell_parent\":{session_uid}}}\n")
}

/// 认 [`parent_preamble`] 那一行。多一个字段、别的 JSON、空 uid 都不认——避免把正常握手行吃掉。
pub fn parse_parent_preamble(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let uid = obj.get("ishell_parent")?.as_u64()?;
    (uid > 0).then_some(uid)
}

/// 沿 `parent` 走到没有父会话的那个 uid。环和过深的链会停住，不会转圈。
pub fn root_of(uid: u64, mut parent: impl FnMut(u64) -> Option<u64>) -> u64 {
    let mut current = uid;
    let mut seen = [0u64; 64];
    for i in 0..64 {
        if seen[..i].contains(&current) {
            return current;
        }
        seen[i] = current;
        match parent(current) {
            Some(p) if p != current => current = p,
            _ => return current,
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_is_one_field_and_rejects_hello() {
        assert_eq!(parse_parent_preamble(&parent_preamble(42)), Some(42));
        assert_eq!(parse_parent_preamble("{\"ishell_parent\":0}\n"), None);
        assert_eq!(
            parse_parent_preamble("{\"ishell_parent\":7,\"hello\":1}"),
            None
        );
        assert_eq!(parse_parent_preamble("{\"hello\":{\"auth\":1}}"), None);
    }

    #[test]
    fn unbind_of_old_generation_keeps_the_new_shell() {
        let first = bind_local(424_242, 1);
        let second = bind_local(424_242, 2);
        unbind_local(424_242, first);
        assert_eq!(lookup(424_242), Some(2));
        unbind_local(424_242, second);
        assert_eq!(lookup(424_242), None);
    }

    #[test]
    fn walk_stops_at_the_registered_shell() {
        let gen = bind_local(10, 4);
        let found = ancestor_session(12, &|pid| match pid {
            12 => Some(11),
            11 => Some(10),
            10 => Some(1),
            _ => None,
        });
        unbind_local(10, gen);
        assert_eq!(found, Some(4));
    }

    #[test]
    fn root_follows_parents_and_stops_on_a_cycle() {
        let parent = |u: u64| match u {
            3 => Some(2),
            2 => Some(1),
            _ => None,
        };
        assert_eq!(root_of(3, parent), 1);
        assert_eq!(root_of(1, parent), 1);
        // 1→2→1：第二次看见 1 就停，停在环的入口。
        let cycle = |u: u64| match u {
            1 => Some(2),
            2 => Some(1),
            _ => None,
        };
        assert_eq!(root_of(1, cycle), 1);
    }
}
