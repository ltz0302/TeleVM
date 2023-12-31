// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use anyhow::bail;
use libc::{c_void, syscall, SYS_mbind};

use anyhow::Result;

/// This function set memory policy for host NUMA node memory range.
///
/// * Arguments
///
/// * `addr` - The memory range starting with addr.
/// * `len` - Length of the memory range.
/// * `mode` - Memory policy mode.
/// * `node_mask` - node_mask specifies physical node ID.
/// * `max_node` - The max node.
/// * `flags` - Mode flags.
pub fn mbind(
    addr: u64,
    len: u64,
    mode: u32,
    node_mask: Vec<u64>,
    max_node: u64,
    flags: u32,
) -> Result<()> {
    let res = unsafe {
        syscall(
            SYS_mbind,
            addr as *mut c_void,
            len,
            mode,
            node_mask.as_ptr(),
            max_node + 1,
            flags,
        )
    };
    if res < 0 {
        bail!(
            "Failed to apply host numa node policy, error is {}",
            std::io::Error::last_os_error()
        );
    }

    Ok(())
}
