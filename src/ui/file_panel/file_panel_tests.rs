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
