//! Can mihomo create a TUN device? It needs CAP_NET_ADMIN, either from running
//! as root or from file capabilities (`setcap cap_net_admin+ep`).

use std::path::Path;

const CAP_NET_ADMIN: u32 = 12;

pub fn can_tun(mihomo: &Path) -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    file_has_cap(mihomo, CAP_NET_ADMIN)
}

/// The command that grants mihomo what TUN mode needs.
pub fn grant_hint(mihomo: &Path) -> String {
    format!(
        "sudo setcap cap_net_admin,cap_net_bind_service+ep {}",
        mihomo.display()
    )
}

fn file_has_cap(path: &Path, cap: u32) -> bool {
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    match xattr::get(&path, "security.capability") {
        Ok(Some(data)) => permitted_has(&data, cap),
        _ => false,
    }
}

/// `struct vfs_cap_data`: magic_etc, then (permitted, inheritable) pairs of
/// u32 for the low and (v2+) high 32 capability bits, little-endian.
fn permitted_has(data: &[u8], cap: u32) -> bool {
    let word = (cap / 32) as usize;
    let offset = 4 + word * 8;
    data.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) & (1 << (cap % 32)) != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vfs_cap_data() {
        // VFS_CAP_REVISION_2 | effective, permitted = net_admin + net_bind_service
        let mut data = vec![0x01, 0x00, 0x00, 0x02];
        data.extend((1u32 << 12 | 1 << 10).to_le_bytes());
        data.extend([0u8; 12]);
        assert!(permitted_has(&data, CAP_NET_ADMIN));
        assert!(!permitted_has(&data, 21));
        assert!(!permitted_has(&[], CAP_NET_ADMIN));
    }
}
