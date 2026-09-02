#[repr(C)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

pub(crate) fn prioritize_generation() {
    let mut attributes = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        sched_policy: 0,
        sched_flags: (libc::SCHED_FLAG_KEEP_POLICY
            | libc::SCHED_FLAG_KEEP_PARAMS
            | libc::SCHED_FLAG_UTIL_CLAMP_MIN) as u64,
        sched_nice: 0,
        sched_priority: 0,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
        sched_util_min: 1024,
        sched_util_max: 0,
    };

    // This is a best-effort hint for the short-lived generator. The kernel may
    // update `size` on E2BIG; every failure otherwise leaves scheduling unchanged.
    unsafe {
        libc::syscall(libc::SYS_sched_setattr, 0, &raw mut attributes, 0);
    }
}
