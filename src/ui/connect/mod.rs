//! 连接对话框：左侧已保存连接列表，右侧录入主机/端口/用户名 + 密码或私钥。

use crate::store::{self, SavedConnection};

mod form;
mod form_widgets;
mod ui;

#[derive(Clone, PartialEq)]
enum AuthKind {
    Password,
    Key,
    // 本机 ssh-agent
    Agent,
    // 键盘交互（支持 OTP / 二次验证）
    Interactive,
}

#[derive(PartialEq)]
enum Mode {
    // 快速连接列表（默认）
    List,
    // 新建 / 编辑表单
    Form,
}

/// 对话框表单状态（在 App 中长期持有）。
pub struct ConnectForm {
    pub open: bool,
    mode: Mode,
    name: String,
    host: String,
    port: String,
    username: String,
    password: String,
    key_path: String,
    passphrase: String,
    auth: AuthKind,
    forward_agent: bool,
    group: String,
    tags: String,
    search: String,
    use_jump: bool,
    j_host: String,
    j_port: String,
    j_username: String,
    j_password: String,
    j_key_path: String,
    j_passphrase: String,
    j_auth: AuthKind,
    error: Option<String>,
    saved: Vec<SavedConnection>,
    sel: Option<usize>,
    confirm_delete: Option<usize>,
    notice: Option<String>,
    import_candidates: Option<Vec<(SavedConnection, bool)>>,
    editing: Option<(String, String)>,
    focus_host: bool,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::List,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            username: "root".into(),
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            auth: AuthKind::Password,
            forward_agent: false,
            group: String::new(),
            tags: String::new(),
            search: String::new(),
            use_jump: false,
            j_host: String::new(),
            j_port: "22".into(),
            j_username: "root".into(),
            j_password: String::new(),
            j_key_path: String::new(),
            j_passphrase: String::new(),
            j_auth: AuthKind::Password,
            error: None,
            saved: store::load(),
            sel: None,
            confirm_delete: None,
            notice: None,
            import_candidates: None,
            editing: None,
            focus_host: false,
        }
    }
}

impl ConnectForm {
    /// 打开对话框：始终回到「快速连接」列表并从磁盘重新加载，确保已保存的连接可见。
    pub fn open_dialog(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        self.saved = store::load();
        self.error = None;
    }

    /// 自检：直接打开到新建表单（仅供截图）。
    pub fn open_form_for_demo(&mut self) {
        self.open = true;
        self.mode = Mode::Form;
        self.reset_form();
        self.focus_host = true;
    }

    /// 自检：打开导入选择对话框（仅供截图）。
    pub fn open_import_demo(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        let mk = |n: &str, h: &str, u: &str, a: &str| {
            (
                SavedConnection {
                    name: n.into(),
                    host: h.into(),
                    port: 22,
                    username: u.into(),
                    auth_kind: a.into(),
                    ..Default::default()
                },
                true,
            )
        };
        self.import_candidates = Some(vec![
            mk("web", "10.0.0.5", "deploy", "key"),
            mk("db", "db.internal", "root", "agent"),
            mk("gw", "gw.example.com", "admin", "password"),
        ]);
    }

    /// 自检：打开分组列表（仅供截图）。
    pub fn open_list_demo(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        let mk = |n: &str, h: &str, u: &str, g: &str, t: &str| SavedConnection {
            name: n.into(),
            host: h.into(),
            port: 22,
            username: u.into(),
            group: g.into(),
            tags: t.into(),
            ..Default::default()
        };
        self.saved = vec![
            mk("生产 Web", "10.0.0.5", "deploy", "生产环境", "web,nginx"),
            mk("生产 DB", "10.0.0.9", "root", "生产环境", "db,mysql"),
            mk("测试机", "192.168.1.20", "test", "测试环境", "qa"),
            mk("跳板机", "gw.example.com", "admin", "测试环境", "bastion"),
            mk("家里 NAS", "192.168.50.2", "nas", "", "home"),
        ];
    }

    /// 自检：打开删除确认对话框（仅供截图）。
    pub fn open_delete_demo(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        self.saved = vec![SavedConnection {
            name: "生产数据库".into(),
            host: "10.0.0.9".into(),
            port: 22,
            username: "root".into(),
            ..Default::default()
        }];
        self.confirm_delete = Some(0);
    }
}
