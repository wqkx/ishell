use super::{normalize_path, path_is_prefix, update_path_trail, FilePanelState};

    #[test]
    fn normalize_trailing_slash() {
        assert_eq!(normalize_path("/home/e5-1/"), "/home/e5-1");
        assert_eq!(normalize_path("/home/e5-1"), "/home/e5-1");
        assert_eq!(normalize_path("/home/e5-1///"), "/home/e5-1");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("///"), "/");
        assert_eq!(normalize_path("  /tmp/  "), "/tmp");
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn path_prefix_and_trail() {
        assert!(path_is_prefix("/", "/a/b"));
        assert!(path_is_prefix("/a", "/a/b/c"));
        assert!(path_is_prefix("/a/b", "/a/b"));
        assert!(!path_is_prefix("/a/b", "/a"));
        assert!(!path_is_prefix("/a", "/ab"));

        let mut s = FilePanelState {
            cwd: "/a".into(),
            nav_prev: "/a/b/c".into(),
            ..Default::default()
        };
        update_path_trail(&mut s);
        assert_eq!(s.path_trail.as_deref(), Some("/a/b/c"));

        // 沿幽灵下钻：保留
        s.cwd = "/a/b".into();
        s.nav_prev = "/a".into();
        update_path_trail(&mut s);
        assert_eq!(s.path_trail.as_deref(), Some("/a/b/c"));

        // 回到幽灵末端：清除
        s.cwd = "/a/b/c".into();
        s.nav_prev = "/a/b".into();
        update_path_trail(&mut s);
        assert!(s.path_trail.is_none());

        // 旁支：清除
        s.path_trail = Some("/a/b/c".into());
        s.cwd = "/x".into();
        s.nav_prev = "/a".into();
        update_path_trail(&mut s);
        assert!(s.path_trail.is_none());
    }

    /// 乱序防护：同一目录多个 List 请求乱序返回时，后到的旧结果（gen 更小）必须被丢弃，
    /// 不得覆盖已应用的较新列表。这是「刷新后外部新建的同名目录不显示、只有过滤框才搜得到」
    /// 那个偶发 bug 的根因防护。
    #[test]
    fn on_listing_drops_stale_out_of_order_results() {
        use crate::proto::FileEntry;
        let ent = |name: &str| FileEntry {
            name: name.into(),
            is_dir: true,
            is_link: false,
            size: 0,
            mtime: 0,
            perm: 0,
            owner: String::new(),
            link_target: None,
            link_dir: false,
        };
        let mut s = FilePanelState::default();
        let p = "/d".to_string();

        // 较新请求的结果先到（gen=8，含 new_dir）
        s.on_listing(p.clone(), vec![ent("new_dir")], 8);
        assert_eq!(s.listings[&p].len(), 1);
        assert_eq!(s.listings[&p][0].name, "new_dir");

        // 较旧请求的陈旧结果后到（gen=5，无 new_dir）：必须丢弃，不得覆盖
        s.on_listing(p.clone(), vec![ent("old_a"), ent("old_b")], 5);
        assert_eq!(s.listings[&p].len(), 1, "陈旧结果不应覆盖较新列表");
        assert_eq!(s.listings[&p][0].name, "new_dir");

        // 更新的请求结果正常应用（gen=12）
        s.on_listing(p.clone(), vec![ent("newest")], 12);
        assert_eq!(s.listings[&p][0].name, "newest");

        // 再来一条比 12 小的陈旧结果（gen=10）：仍丢弃
        s.on_listing(p.clone(), vec![ent("stale")], 10);
        assert_eq!(s.listings[&p][0].name, "newest");
    }
