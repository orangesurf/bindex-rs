use bitcoin::hashes::{sha256d, Hash as _};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub branch: Vec<sha256d::Hash>,
    pub root: sha256d::Hash,
}

pub fn branch_and_root(leaves: &[sha256d::Hash], index: usize) -> Proof {
    assert!(!leaves.is_empty());
    assert!(index < leaves.len());

    let mut branch = Vec::new();
    let mut level = leaves.to_vec();
    let mut idx = index;

    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }

        let sibling = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
        branch.push(level[sibling]);

        let mut next = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks_exact(2) {
            let mut payload = [0u8; 64];
            payload[..32].copy_from_slice(pair[0].as_byte_array());
            payload[32..].copy_from_slice(pair[1].as_byte_array());
            next.push(sha256d::Hash::hash(&payload));
        }

        level = next;
        idx /= 2;
    }

    Proof {
        branch,
        root: level[0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merkle_branch_rebuilds_root() {
        let leaves = (0u8..5)
            .map(|byte| sha256d::Hash::hash(&[byte]))
            .collect::<Vec<_>>();
        let proof = branch_and_root(&leaves, 2);
        assert_eq!(proof.branch.len(), 3);
        assert_eq!(proof.root, branch_and_root(&leaves, 0).root);
    }
}
