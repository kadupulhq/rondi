//! The `TreeDepth` statistic: the height of a height-balanced (AVL) binary
//! search tree holding the names of the cache entries (RC-55).
//!
//! The cache itself is a hash map; this tree exists only to report the
//! height the statistic describes. Names are inserted as entries are
//! created and removed as they leave, with the textbook AVL insertion and
//! deletion (a removed inner node takes its in-order successor's place).

use std::cmp::Ordering;

#[derive(Default)]
pub(crate) struct Depth {
    root: Link,
}

type Link = Option<Box<Node>>;

struct Node {
    key: Vec<u8>,
    height: u32,
    left: Link,
    right: Link,
}

fn height(link: &Link) -> u32 {
    link.as_ref().map_or(0, |node| node.height)
}

impl Node {
    fn leaf(key: Vec<u8>) -> Box<Self> {
        Box::new(Self {
            key,
            height: 1,
            left: None,
            right: None,
        })
    }

    fn update(&mut self) {
        self.height = 1 + height(&self.left).max(height(&self.right));
    }

    fn balance(&self) -> i64 {
        i64::from(height(&self.left)) - i64::from(height(&self.right))
    }
}

fn rotate_right(mut node: Box<Node>) -> Box<Node> {
    let mut pivot = node.left.take().expect("a left child to rotate");
    node.left = pivot.right.take();
    node.update();
    pivot.right = Some(node);
    pivot.update();
    pivot
}

fn rotate_left(mut node: Box<Node>) -> Box<Node> {
    let mut pivot = node.right.take().expect("a right child to rotate");
    node.right = pivot.left.take();
    node.update();
    pivot.left = Some(node);
    pivot.update();
    pivot
}

fn rebalance(mut node: Box<Node>) -> Box<Node> {
    node.update();
    let balance = node.balance();
    if balance > 1 {
        if node.left.as_ref().is_some_and(|left| left.balance() < 0) {
            node.left = node.left.take().map(rotate_left);
        }
        return rotate_right(node);
    }
    if balance < -1 {
        if node.right.as_ref().is_some_and(|right| right.balance() > 0) {
            node.right = node.right.take().map(rotate_right);
        }
        return rotate_left(node);
    }
    node
}

fn insert(link: Link, key: &[u8]) -> Box<Node> {
    let Some(mut node) = link else {
        return Node::leaf(key.to_vec());
    };
    match key.cmp(&node.key) {
        Ordering::Less => node.left = Some(insert(node.left.take(), key)),
        Ordering::Greater => node.right = Some(insert(node.right.take(), key)),
        Ordering::Equal => return node,
    }
    rebalance(node)
}

/// Removes the smallest key below `node`, returning it and the rest.
fn take_min(mut node: Box<Node>) -> (Vec<u8>, Link) {
    match node.left.take() {
        None => (std::mem::take(&mut node.key), node.right.take()),
        Some(left) => {
            let (key, rest) = take_min(left);
            node.left = rest;
            (key, Some(rebalance(node)))
        }
    }
}

fn remove(link: Link, key: &[u8]) -> Link {
    let mut node = link?;
    match key.cmp(&node.key) {
        Ordering::Less => node.left = remove(node.left.take(), key),
        Ordering::Greater => node.right = remove(node.right.take(), key),
        Ordering::Equal => match (node.left.take(), node.right.take()) {
            (None, None) => return None,
            (Some(child), None) | (None, Some(child)) => return Some(child),
            (Some(left), Some(right)) => {
                let (successor, rest) = take_min(right);
                node.key = successor;
                node.left = Some(left);
                node.right = rest;
            }
        },
    }
    Some(rebalance(node))
}

impl Depth {
    pub(crate) fn insert(&mut self, key: &[u8]) {
        self.root = Some(insert(self.root.take(), key));
    }

    pub(crate) fn remove(&mut self, key: &[u8]) {
        self.root = remove(self.root.take(), key);
    }

    pub(crate) fn height(&self) -> u32 {
        height(&self.root)
    }
}

impl Drop for Depth {
    // Dropping a deep tree node by node keeps the stack shallow; an AVL
    // tree is never deep, but this costs nothing.
    fn drop(&mut self) {
        let mut stack = Vec::new();
        stack.extend(self.root.take());
        while let Some(mut node) = stack.pop() {
            stack.extend(node.left.take());
            stack.extend(node.right.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(link: &Link) -> u32 {
        let Some(node) = link else { return 0 };
        let (left, right) = (check(&node.left), check(&node.right));
        assert!(left.abs_diff(right) <= 1, "unbalanced at {:?}", node.key);
        if let Some(child) = &node.left {
            assert!(child.key < node.key);
        }
        if let Some(child) = &node.right {
            assert!(child.key > node.key);
        }
        assert_eq!(node.height, 1 + left.max(right));
        node.height
    }

    #[test]
    fn heights_follow_the_spec_examples() {
        let mut depth = Depth::default();
        assert_eq!(depth.height(), 0);
        for (count, expected) in [(1, 1), (2, 2), (3, 2), (4, 3)] {
            depth.insert(format!("k{count}").as_bytes());
            assert_eq!(depth.height(), expected);
        }
        depth.insert(b"k1");
        assert_eq!(depth.height(), 3);
        depth.remove(b"k2");
        assert_eq!(depth.height(), 2);
        depth.remove(b"missing");
        assert_eq!(depth.height(), 2);
    }

    #[test]
    fn random_insertions_and_removals_stay_balanced() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut depth = Depth::default();
        let mut keys = std::collections::BTreeSet::new();
        for _ in 0..4000 {
            let key = format!("{}", next() % 500).into_bytes();
            if next() % 3 == 0 {
                depth.remove(&key);
                keys.remove(&key);
            } else {
                depth.insert(&key);
                keys.insert(key);
            }
            assert_eq!(check(&depth.root), depth.height());
        }
        let count = keys.len() as f64;
        assert!(f64::from(depth.height()) <= 1.45 * (count + 2.0).log2());
    }
}
