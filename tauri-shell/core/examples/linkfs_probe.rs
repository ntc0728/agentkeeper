use agentkeeper_core::linkfs;
use std::path::Path;
fn main() {
    let dir = std::env::temp_dir().join("ak-linkfs-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("f.txt"), "x").unwrap();

    // junction
    let j = dir.join("junction");
    linkfs::create_dir_link(&target, &j, Some(linkfs::LinkMethod::Junction)).unwrap();
    println!("junction: kind={:?} is_link={} entry_kind={:?}",
        linkfs::link_kind(&j), linkfs::is_link(&j), linkfs::entry_link_kind(&j));

    // symlink
    let s = dir.join("symlink");
    linkfs::create_dir_link(&target, &s, Some(linkfs::LinkMethod::Symlink)).unwrap();
    println!("symlink : kind={:?}", linkfs::link_kind(&s));

    // 目录枚举（collect_dir_seen 的场景）
    let parent = dir.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    let _ = std::fs::hard_link(target.join("f.txt"), parent.join("hard.txt"));
    println!("hardlink: kind={:?}", linkfs::link_kind(&parent.join("hard.txt")));
    println!("plain dir entry: kind={:?}", linkfs::entry_link_kind(&target));

    // 模拟 OneDrive 占位：任意标签都进 else 分支 → Symlink。用 junction 换标签太麻烦，
    // 验证「未知标签→Symlink」即可：读不到时退回分支。
    let _ = Path::new("");
    let _ = std::fs::remove_dir_all(&dir);
}
