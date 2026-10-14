//! 内存目录树：MFT 全量扫描的产物，USN 增量更新维护其实时性。

use std::collections::HashMap;

/// NTFS 每个卷的根目录 FRN 固定为 5
pub const ROOT_FRN: u64 = 5;

/// 树节点
#[derive(Debug, Clone)]
pub struct Node {
    /// 父目录 FRN
    pub parent: u64,
    pub name: String,
    pub is_dir: bool,
    /// 系统文件 / 索引产物等噪声节点（默认不参与搜索）
    pub hidden: bool,
    pub size: u64,
}

/// 单卷内存树
pub struct Tree {
    drive: String,
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, Vec<u64>>,
    /// FRN -> 完整路径缓存
    paths: HashMap<u64, String>,
}

impl Tree {
    pub fn new(drive: &str) -> Self {
        Self {
            drive: drive.to_string(),
            nodes: HashMap::new(),
            children: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub fn drive(&self) -> &str {
        &self.drive
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 插入 / 更新节点（USN 新建、MFT 扫描）
    pub fn upsert(&mut self, frn: u64, node: Node) {
        if let Some((old_parent, old_name)) = self
            .nodes
            .get(&frn)
            .map(|old| (old.parent, old.name.clone()))
        {
            if old_parent != node.parent || old_name != node.name {
                self.detach(frn, old_parent, &old_name);
            }
        }
        self.children
            .entry(node.parent)
            .or_default()
            .push(frn);
        self.paths.remove(&frn);
        self.nodes.insert(frn, node);
    }

    /// 删除节点（连带子树）
    pub fn remove(&mut self, frn: u64) {
        let Some(node) = self.nodes.remove(&frn) else {
            return;
        };
        self.detach(frn, node.parent, &node.name);
        self.paths.remove(&frn);

        let children = self.children.remove(&frn).unwrap_or_default();
        for child in children {
            self.remove(child);
        }
    }

    fn detach(&mut self, frn: u64, parent: u64, name: &str) {
        if let Some(list) = self.children.get_mut(&parent) {
            list.retain(|child| *child != frn);
        }
        // 父节点改名时旧路径缓存失效
        if !name.is_empty() {
            self.paths.retain(|_, path| !path.ends_with(name));
        }
    }

    /// 重命名 / 移动（USN RENAME_NEW_NAME）
    pub fn rename(&mut self, frn: u64, new_parent: u64, new_name: String) {
        let Some(node) = self.nodes.get(&frn).cloned() else {
            return;
        };
        self.detach(frn, node.parent, &node.name);
        self.paths.remove(&frn);
        let mut updated = node;
        updated.parent = new_parent;
        updated.name = new_name;
        self.children.entry(new_parent).or_default().push(frn);
        self.nodes.insert(frn, updated);
    }

    pub fn get(&self, frn: u64) -> Option<&Node> {
        self.nodes.get(&frn)
    }

    /// 计算完整路径（带缓存）
    pub fn path_of(&mut self, frn: u64) -> Option<String> {
        if let Some(path) = self.paths.get(&frn) {
            return Some(path.clone());
        }
        let mut segments = Vec::new();
        let mut current = frn;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 128 {
                return None;
            }
            if current == ROOT_FRN {
                break;
            }
            let Some(node) = self.nodes.get(&current) else {
                return None;
            };
            segments.push(node.name.clone());
            if node.parent == current {
                return None;
            }
            current = node.parent;
        }
        segments.reverse();
        let path = format!("{}\\{}", self.drive, segments.join("\\"));
        self.paths.insert(frn, path.clone());
        Some(path)
    }

    /// 便捷方法：插入后立即取完整路径
    #[allow(dead_code)]
    pub fn insert_with_path(&mut self, frn: u64, node: Node) -> Option<String> {
        self.upsert(frn, node);
        self.path_of(frn)
    }

    /// 遍历所有节点（搜索用）
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Node)> {
        self.nodes.iter().map(|(frn, node)| (*frn, node))
    }
}
