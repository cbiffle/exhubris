#![no_std]
#![allow(unexpected_cfgs)] // for now

pub enum Sysnum {
    Send = 0,
    Recv = 1,
    Reply = 2,
    SetTimer = 3,
    Panic = 8,
    GetTimer = 9,
    ReplyFault = 12,
}

pub const GEN_BITS: u32 = 6;

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct TaskId(u16);

impl TaskId {
    pub const KERNEL: TaskId = Self(!0);

    pub const fn new(task_index: u16, generation: Gen) -> Self {
        Self(task_index & ((1 << GEN_BITS) - 1) | ((generation.0 as u16) << (16 - GEN_BITS)))
    }

    pub const fn gen0(task_index: u16) -> Self {
        Self::new(task_index, Gen::DEFAULT)
    }

    pub const fn with_generation(self, generation: Gen) -> Self {
        Self::new(self.task_index(), generation)
    }

    pub const fn task_index(self) -> u16 {
        self.0 & ((1 << GEN_BITS) - 1)
    }
}

impl From<TaskId> for u16 {
    fn from(tid: TaskId) -> u16 {
        tid.0
    }
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Default)]
pub struct Gen(u8);

impl Gen {
    pub const DEFAULT: Self = Self(0);
}

#[repr(transparent)]
pub struct Lease<'a> {
    inner: AbiLease,
    _marker: PhantomData<&'a mut ()>,
}

#[repr(C)]
struct AbiLease {
    attributes: u32,
    base_address: *const u8,
    length: usize,
}

pub fn idle() {
    arch::idle();
}

cfg_if::cfg_if! {
    if #[cfg(any(
        hubris_target = "thumbv7em-none-eabihf",
        hubris_target = "thumbv6m-none-eabi",
    ))] {
        #[path = "arch/arm_m.rs"]
        mod arch;
    } else {
        #[path = "arch/fake.rs"]
        mod arch;
    }
}

use core::marker::PhantomData;

#[doc(inline)]
pub use self::arch::sys_panic;

#[doc(inline)]
pub use self::arch::sys_send;

#[doc(inline)]
pub use self::arch::sys_recv;

#[doc(inline)]
pub use self::arch::sys_recv_open;

#[doc(inline)]
pub use self::arch::sys_recv_notification;

#[doc(inline)]
pub use self::arch::sys_reply;

#[doc(inline)]
pub use self::arch::sys_reply_fault;

#[doc(inline)]
pub use self::arch::sys_set_timer;

#[doc(inline)]
pub use self::arch::sys_get_timer;

#[derive(Copy, Clone, Debug)]
pub struct ResponseCode(u32);

impl ResponseCode {
    pub const SUCCESS: Self = Self(0);
}

#[derive(Copy, Clone, Debug)]
pub struct TaskDeath {
    now: Gen,
}

impl TaskDeath {
    pub fn new_generation(self) -> Gen {
        self.now
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NotADeadCode;

impl TryFrom<ResponseCode> for TaskDeath {
    type Error = NotADeadCode;
    fn try_from(rc: ResponseCode) -> Result<Self, Self::Error> {
        if rc.0 & 0xFFFF_FF00 == 0xFFFF_FF00 {
            Ok(TaskDeath { now: Gen(rc.0 as u8) })
        } else {
            Err(NotADeadCode)
        }
    }
}

#[derive(Debug)]
pub struct RecvMessage<'a> {
    pub sender: TaskId,
    pub operation_or_notification: u32,
    pub data: Result<&'a mut [u8], Truncated>,
    pub reply_capacity: usize,
    pub lease_count: usize,
}

#[derive(Copy, Clone, Debug)]
pub struct Truncated;

#[derive(Copy, Clone, Debug)]
pub struct TimerSettings {
    pub now: u64,
    pub alarm: Option<(u64, u32)>,
}

#[derive(Copy, Clone, Debug)]
pub enum ReplyFaultReason {
    /// The message indicated some operation number that is unknown to the
    /// server -- which almost certainly indicates that the client intended the
    /// message for a different kind of server.
    UndefinedOperation = 0,
    /// The message sent by the client had the wrong size to even attempt
    /// parsing by the server -- either too short or too long. (Because most
    /// messages are fixed size, it currently doesn't seem useful to distinguish
    /// between too-short and too-long.)
    BadMessageSize = 1,
    /// The server attempted to parse the message, and couldn't. This may
    /// indicate an enum with an illegal value, or a more nuanced error on
    /// operations that use serde encoding.
    BadMessageContents = 2,
    /// The client did not provide the leases required for the operation, or
    /// provided them with the wrong attributes.
    BadLeases = 3,
    /// The client did not provide a reply buffer large enough to receive the
    /// server's reply, despite this information being implied by the IPC
    /// protocol.
    ReplyBufferTooSmall = 4,

    /// Application-defined: The client attempted to operate on a resource that
    /// is not available to them due to mandatory access control or other type
    /// of access validation.
    AccessViolation = 5,
}

cfg_if::cfg_if! {
    if #[cfg(feature = "no-panic")] {
        #[panic_handler]
        fn panic(_: &core::panic::PanicInfo<'_>) -> !{
            extern "C" {
                fn you_have_introduced_a_panic_but_this_task_has_disabled_panics() -> !;
            }

            // Safety: this function doesn't exist, deliberately. We call it to
            // cause a link error. So, while it _appears_ unsafe here, there is
            // no contract we need to meet.
            unsafe {
                you_have_introduced_a_panic_but_this_task_has_disabled_panics()
            }
        }
    } else if #[cfg(feature = "no-panic-messages")] {
        #[panic_handler]
        fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
            sys_panic(b"PANIC")
        }
    } else {
        compile_error!("panic support not yet implemented");
    }
}

include!(concat!(env!("OUT_DIR"), "/hubris_abi_version.rs"));
