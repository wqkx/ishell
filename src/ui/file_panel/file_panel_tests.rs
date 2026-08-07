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

/// 目录必须始终排在文件前面，升降序只反转组内顺序、不跨组搬动。
///
/// 0.17.0 的回归在这里有个陷阱：排序键写反(文件在前)**同时**降序分支的 `dir_end`
/// 算成 0 把整个数组翻转，两个错误在降序下互相抵消——只测一个方向会全绿。所以三个
/// 排序键 × 升降两个方向都得断言。
#[test]
fn sort_puts_dirs_first_in_both_directions() {
    use crate::proto::FileEntry;
    use super::list::sort_entries;
    use super::SortKey;

    let mk = |name: &str, is_dir: bool, size: u64, mtime: u64| FileEntry {
        name: name.into(),
        is_dir,
        is_link: false,
        size,
        mtime,
        perm: 0,
        owner: String::new(),
        link_target: None,
        link_dir: false,
    };
    // 故意打乱输入顺序，且让 size/mtime 的大小关系与名字顺序不一致
    let base = vec![
        mk("b_file", false, 30, 300),
        mk("a_dir", true, 10, 100),
        mk("a_file", false, 40, 400),
        mk("b_dir", true, 20, 200),
    ];
    let names = |v: &[FileEntry]| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();

    for key in [SortKey::Name, SortKey::Size, SortKey::Mtime] {
        for desc in [false, true] {
            let mut v = base.clone();
            sort_entries(&mut v, key, desc);
            assert_eq!(
                v.iter().take_while(|e| e.is_dir).count(),
                2,
                "两个目录必须都在最前面：key={key:?} desc={desc}"
            );
        }
    }

    // 升序：目录按名升序在前，文件按名升序在后
    let mut v = base.clone();
    sort_entries(&mut v, SortKey::Name, false);
    assert_eq!(names(&v), ["a_dir", "b_dir", "a_file", "b_file"]);

    // 降序：仍是目录在前，只是各组内反过来
    let mut v = base.clone();
    sort_entries(&mut v, SortKey::Name, true);
    assert_eq!(names(&v), ["b_dir", "a_dir", "b_file", "a_file"]);

    // 按大小：目录组内 10 < 20，文件组内 30 < 40（升序）
    let mut v = base.clone();
    sort_entries(&mut v, SortKey::Size, false);
    assert_eq!(names(&v), ["a_dir", "b_dir", "b_file", "a_file"]);
    let mut v = base.clone();
    sort_entries(&mut v, SortKey::Size, true);
    assert_eq!(names(&v), ["b_dir", "a_dir", "a_file", "b_file"]);

    // 按时间：与 size 同构（mtime 递增顺序一致）
    let mut v = base.clone();
    sort_entries(&mut v, SortKey::Mtime, false);
    assert_eq!(names(&v), ["a_dir", "b_dir", "b_file", "a_file"]);

    // 全是文件时降序也不能出错（旧代码的 dir_end 恰好在这种情况下"蒙对"）
    let mut only_files = vec![mk("x", false, 1, 1), mk("y", false, 2, 2)];
    sort_entries(&mut only_files, SortKey::Name, true);
    assert_eq!(names(&only_files), ["y", "x"]);

    // 全是目录时同理
    let mut only_dirs = vec![mk("x", true, 1, 1), mk("y", true, 2, 2)];
    sort_entries(&mut only_dirs, SortKey::Name, true);
    assert_eq!(names(&only_dirs), ["y", "x"]);
}

/// 用户报过的偶发 bug：删掉一个文件夹、外部又重建了同名的，点刷新却不显示（过滤框反倒
/// 能搜到）。根因是 `view_cache` 只认 `applied_list_gen`，而 `refresh_dir` 这类原地改动
/// 不推进它——过滤框改的是缓存键的另一个字段，所以一按就重建了，才有那个"搜得到"的怪象。
#[test]
fn listings_mutations_invalidate_view_cache() {
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
    s.cwd = p.clone();

    s.on_listing(p.clone(), vec![ent("keep")], 1);
    let after_listing = s.view_epoch;

    // 手动刷新：移除该目录缓存 → 必须让视图缓存失效，否则界面继续画旧列表
    assert!(s.refresh_dir(&p));
    assert_ne!(s.view_epoch, after_listing, "refresh_dir 未使视图缓存失效");
    let after_refresh = s.view_epoch;

    // 外部重建的同名目录随刷新结果回来：同样要失效
    s.on_listing(p.clone(), vec![ent("keep"), ent("recreated")], 2);
    assert_ne!(s.view_epoch, after_refresh, "on_listing 未使视图缓存失效");
    assert_eq!(s.listings[&p].len(), 2);
    let after_second = s.view_epoch;

    // 新建目录的乐观插入：也必须失效，否则新建的文件夹要等一个来回才看得见
    s.insert_new(&p, "brand_new", true);
    assert_ne!(s.view_epoch, after_second, "insert_new 未使视图缓存失效");
    assert!(s.listings[&p].iter().any(|e| e.name == "brand_new"));
}
